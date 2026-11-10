//! 安裝資料夾的修改權判定，以及 helper 寫入時改用的「不超過啟動者一般身分」的執行緒身分。parent 據此決定
//! 國際服更新在程序內套用或經提權 helper；helper 據此決定以什麼身分寫入——一般權限就能修改的安裝裡，一般權限的
//! 程序可以在 helper 檢查 junction（`zipatch::target::ensure_no_link`）與實際寫入之間把子目錄換成 junction，
//! 以系統管理員權限寫入就會被導到受保護的位置，所以那種安裝改用 [`Impersonation`] 寫入。
//!
//! 以 `AccessCheck` 依資料夾的 security descriptor（擁有者、群組、DACL、完整性標籤）算出指定身分的有效
//! 權限，不在遊戲目錄建立或刪除任何檔案；parent 與 helper 用同一個判定，結論一致。本程式只發 x64，64 位元
//! 程序沒有 UAC 檔案虛擬化，判定與實際寫入的結果相同。

use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::{
    AccessCheck, CreateRestrictedToken, DACL_SECURITY_INFORMATION, DISABLE_MAX_PRIVILEGE, DuplicateToken,
    DuplicateTokenEx, EqualSid, GENERIC_MAPPING, GROUP_SECURITY_INFORMATION, GetFileSecurityW, GetTokenInformation,
    LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PRIVILEGE_SET, PSECURITY_DESCRIPTOR, RevertToSelf,
    SID_AND_ATTRIBUTES, SecurityIdentification, SecurityImpersonation, SetTokenInformation, TOKEN_ADJUST_DEFAULT,
    TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_ELEVATION_TYPE, TOKEN_GROUPS, TOKEN_IMPERSONATE,
    TOKEN_INFORMATION_CLASS, TOKEN_LINKED_TOKEN, TOKEN_QUERY, TOKEN_USER, TokenDefaultDacl, TokenElevation,
    TokenElevationType, TokenElevationTypeFull, TokenGroups, TokenImpersonation, TokenIntegrityLevel,
    TokenLinkedToken, TokenOwner, TokenRestrictedSids, TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ALL_ACCESS, FILE_DELETE_CHILD,
    FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, WRITE_DAC, WRITE_OWNER,
};
use windows::Win32::System::SystemServices::{
    MAXIMUM_ALLOWED, SE_GROUP_ENABLED, SE_GROUP_INTEGRITY, SE_GROUP_USE_FOR_DENY_ONLY,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken, SetThreadToken};
use windows::core::{BOOL, HSTRING};

use crate::error::AppResult;
use crate::patch::running::{Win32Handle, win_error};

/// 能改動資料夾內容或替換子目錄的權限：新增檔案、新增子目錄（含 junction）、刪除或改名子項、刪除或改名
/// 自身、改 DACL 或擁有者（之後可給自己任何權限）。任一項即算可修改。
const MODIFY_RIGHTS: u32 = FILE_ADD_FILE.0
    | FILE_ADD_SUBDIRECTORY.0
    | FILE_DELETE_CHILD.0
    | DELETE.0
    | WRITE_DAC.0
    | WRITE_OWNER.0;

/// `AccessCheck` 用的身分：identification 層級的 impersonation token，Drop 時關閉。
pub(crate) struct AccessToken(Win32Handle);

impl AccessToken {
    /// 目前程序自己的身分。parent 用來判定能否在程序內直接套用。
    pub(crate) fn current() -> AppResult<Self> {
        duplicate(&process_token()?)
    }

    /// 啟動 helper 的一般權限身分。helper 經 UAC 同意提權時，程序 token 是 UAC 分出的完整 token，回它連結的
    /// 受限 token（`TokenLinkedToken`，同一次登入未提權的身分）；不是這種 token（沒有提權、UAC 關閉、帳號
    /// 沒有分割 token）時回 `None`：沒有可直接取得、比目前程序低的同使用者身分。
    pub(crate) fn linked() -> AppResult<Option<Self>> {
        let process = process_token()?;
        let mut elevation = TOKEN_ELEVATION_TYPE::default();
        let mut len = 0u32;
        // SAFETY: process 含 TOKEN_QUERY；輸出緩衝區是本函式存活中的區域變數，長度即其大小。
        // quality-allow: Win32 FFI，GetTokenInformation 沒有安全包裝
        unsafe {
            GetTokenInformation(
                process.get(),
                TokenElevationType,
                Some((&raw mut elevation).cast()),
                size_of::<TOKEN_ELEVATION_TYPE>() as u32,
                &mut len,
            )
        }
        .map_err(|e| win_error("讀取程序的提權狀態", e))?;
        if elevation != TokenElevationTypeFull {
            return Ok(None);
        }
        let mut linked = TOKEN_LINKED_TOKEN::default();
        // SAFETY: 同上；成功時 LinkedToken 是新開的 handle，交給 Win32Handle 獨占。
        // quality-allow: Win32 FFI，GetTokenInformation 沒有安全包裝
        unsafe {
            GetTokenInformation(
                process.get(),
                TokenLinkedToken,
                Some((&raw mut linked).cast()),
                size_of::<TOKEN_LINKED_TOKEN>() as u32,
                &mut len,
            )
        }
        .map_err(|e| win_error("取得未提權的身分", e))?;
        // 沒有 SeTcbPrivilege 的呼叫者拿到的連結 token 是 identification 層級的 impersonation token，
        // 可以直接交給 AccessCheck，但不能拿來模擬（見 [`Impersonation`]）。
        Ok(Some(Self(Win32Handle::new(linked.LinkedToken))))
    }

    /// 判定用的一般權限身分：[`Self::linked`]，沒有時用目前程序自己的身分（影子 helper 判定影子上層）。
    pub(crate) fn unelevated() -> AppResult<Self> {
        match Self::linked()? {
            Some(linked) => Ok(linked),
            None => Self::current(),
        }
    }
}

/// 目前執行緒改用「不超過 `reference` 的身分」，Drop 時 `RevertToSelf`。綁在建立它的執行緒上（不是 `Send`），
/// 只在 helper 單執行緒寫入時使用。
pub(crate) struct Impersonation {
    _token: Win32Handle,
    _thread_bound: PhantomData<*const ()>,
}

impl Impersonation {
    /// 從目前程序的 token 縮減出不超過 `reference`（helper 的 linked token）的身分並套到目前執行緒：
    /// 1. `reference` 必須和目前程序是同一使用者 SID，且沒有 restricting SID。
    /// 2. `CreateRestrictedToken(DISABLE_MAX_PRIVILEGE)`：特權只留 SeChangeNotify；使用者 SID 在 `reference` 是
    ///    僅拒絕時改成僅拒絕（`TOKEN_USER` 沒有「啟用」旗標，不看）；每個群組（完整性標籤除外）在 `reference` 裡
    ///    不是「啟用、且非僅拒絕」就改成僅拒絕。結果能用於授權的 SID 是 `reference` 的子集。
    /// 3. `DuplicateTokenEx` 成 `SecurityImpersonation` 層級的 impersonation token，完整性等級、預設擁有者、
    ///    預設 DACL 改成 `reference` 的值（新檔擁有者是使用者；完整性降到 `reference` 的等級）。
    /// 4. `SetThreadToken` 套到目前執行緒。
    ///
    /// 任一步失敗回錯誤，執行緒身分不變；呼叫端不寫入。
    pub(crate) fn begin(reference: &AccessToken) -> AppResult<Self> {
        let own_token = process_token()?;
        let reference_token = &reference.0;
        let restricting = token_info(reference_token, TokenRestrictedSids)?;
        // SAFETY: 緩衝區由 GetTokenInformation 以 TokenRestrictedSids 填好、8 位元組對齊，開頭是 TOKEN_GROUPS。
        // quality-allow: 讀 Win32 token 結構
        if unsafe { (*restricting.as_ptr().cast::<TOKEN_GROUPS>()).GroupCount } != 0 {
            return Err(untrusted("參考身分是受限 token"));
        }
        let own = TokenSids::read(&own_token)?;
        let other = TokenSids::read(reference_token)?;
        // SAFETY: 兩個 SID 指標都指向各自 TokenSids 持有、仍存活的緩衝區。
        // quality-allow: Win32 FFI，EqualSid 沒有安全包裝；不相等或失敗都回錯
        if unsafe { EqualSid(own.user().Sid, other.user().Sid) }.is_err() {
            return Err(untrusted("參考身分不是同一個使用者"));
        }
        let sids = sids_to_disable(&own, &other);
        let mut restricted = HANDLE::default();
        // SAFETY: own_token 含 TOKEN_DUPLICATE；sids 的 SID 指標指向 own 的緩衝區，呼叫期間存活；restricted 成功時
        // 交給 Win32Handle 獨占。quality-allow: Win32 FFI，CreateRestrictedToken 沒有安全包裝
        unsafe {
            CreateRestrictedToken(
                own_token.get(),
                DISABLE_MAX_PRIVILEGE,
                Some(&sids),
                None,
                None,
                &mut restricted,
            )
        }
        .map_err(|e| win_error("建立縮減的身分", e))?;
        let restricted = Win32Handle::new(restricted);
        let mut impersonation = HANDLE::default();
        // SAFETY: restricted 與 own_token 有相同存取權（含 TOKEN_DUPLICATE）；impersonation 成功時交給 Win32Handle。
        // quality-allow: Win32 FFI，DuplicateTokenEx 沒有安全包裝
        unsafe {
            DuplicateTokenEx(
                restricted.get(),
                TOKEN_IMPERSONATE | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT,
                None,
                SecurityImpersonation,
                TokenImpersonation,
                &mut impersonation,
            )
        }
        .map_err(|e| win_error("複製縮減的身分", e))?;
        let impersonation = Win32Handle::new(impersonation);
        for class in [TokenIntegrityLevel, TokenOwner, TokenDefaultDacl] {
            let value = token_info(reference_token, class)?;
            // SAFETY: value 是 GetTokenInformation 以同一資訊類別填好的緩衝區（內部指標指向它自己），呼叫期間存活；
            // impersonation 含 TOKEN_ADJUST_DEFAULT。quality-allow: Win32 FFI，SetTokenInformation 沒有安全包裝
            unsafe {
                SetTokenInformation(
                    impersonation.get(),
                    class,
                    value.as_ptr().cast(),
                    (value.len() * size_of::<u64>()) as u32,
                )
            }
            .map_err(|e| win_error("設定縮減的身分", e))?;
        }
        // SAFETY: None＝目前執行緒；impersonation 含 TOKEN_IMPERSONATE。
        // quality-allow: Win32 FFI，SetThreadToken 沒有安全包裝
        unsafe { SetThreadToken(None, Some(impersonation.get())) }
            .map_err(|e| win_error("套用縮減的身分", e))?;
        Ok(Self {
            _token: impersonation,
            _thread_bound: PhantomData,
        })
    }
}

impl Drop for Impersonation {
    fn drop(&mut self) {
        // SAFETY: 只結束目前執行緒的模擬。
        // quality-allow: Win32 FFI；還原失敗時 helper 只剩結束，Drop 無法回傳錯誤
        let _ = unsafe { RevertToSelf() };
    }
}

/// 在沒有模擬的執行緒執行 `f`：新執行緒以程序自己的 token 執行，不繼承呼叫端執行緒的 [`Impersonation`]。helper
/// 以縮減的身分寫入時，遊戲執行判定（只列舉程序、不寫入）用它以 helper 自己的權限做，開得了本程式以系統管理員
/// 身分啟動的另一服遊戲。
pub(crate) fn unimpersonated<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| match scope.spawn(f).join() {
        Ok(value) => value,
        Err(panic) => std::panic::resume_unwind(panic),
    })
}

/// token 的使用者 SID 與群組（`GetTokenInformation` 的緩衝區，SID 指標指向它們自己）。
struct TokenSids {
    user: Vec<u64>,
    groups: Vec<u64>,
}

impl TokenSids {
    fn read(token: &Win32Handle) -> AppResult<Self> {
        Ok(Self {
            user: token_info(token, TokenUser)?,
            groups: token_info(token, TokenGroups)?,
        })
    }

    fn user(&self) -> SID_AND_ATTRIBUTES {
        // SAFETY: 緩衝區由 GetTokenInformation 以 TokenUser 填好、8 位元組對齊，開頭是 TOKEN_USER。
        // quality-allow: 讀 Win32 token 結構
        unsafe { (*self.user.as_ptr().cast::<TOKEN_USER>()).User }
    }

    fn groups(&self) -> &[SID_AND_ATTRIBUTES] {
        let header = self.groups.as_ptr().cast::<TOKEN_GROUPS>();
        // SAFETY: 緩衝區由 GetTokenInformation 以 TokenGroups 填好、8 位元組對齊；群組陣列接在 TOKEN_GROUPS 之後共
        // GroupCount 項，指標由整個緩衝區導出，借用期間緩衝區不變。quality-allow: 讀 Win32 token 結構
        unsafe {
            let first = (&raw const (*header).Groups).cast::<SID_AND_ATTRIBUTES>();
            std::slice::from_raw_parts(first, (*header).GroupCount as usize)
        }
    }
}

/// [`Impersonation`] 第 2 步：`own` 的 SID 中要改成僅拒絕的（指標指向 `own` 的緩衝區）。
fn sids_to_disable(own: &TokenSids, reference: &TokenSids) -> Vec<SID_AND_ATTRIBUTES> {
    let deny_only = SE_GROUP_USE_FOR_DENY_ONLY as u32;
    let usable = |g: &SID_AND_ATTRIBUTES| g.Attributes & SE_GROUP_ENABLED as u32 != 0 && g.Attributes & deny_only == 0;
    let user = own.user();
    let mut out: Vec<SID_AND_ATTRIBUTES> = Vec::new();
    if reference.user().Attributes & deny_only != 0 {
        out.push(SID_AND_ATTRIBUTES { Sid: user.Sid, Attributes: 0 });
    }
    for group in own.groups() {
        if group.Attributes & SE_GROUP_INTEGRITY as u32 != 0 {
            continue;
        }
        let granted = reference.groups().iter().any(|r| {
            // SAFETY: 兩個 SID 指標都指向仍存活的 TokenSids 緩衝區。
            // quality-allow: Win32 FFI，EqualSid 沒有安全包裝；失敗視為不相等（改成僅拒絕）
            usable(r) && unsafe { EqualSid(r.Sid, group.Sid) }.is_ok()
        });
        if !granted {
            out.push(SID_AND_ATTRIBUTES { Sid: group.Sid, Attributes: 0 });
        }
    }
    out
}

fn untrusted(reason: &str) -> crate::error::AppError {
    crate::error::AppError::Install {
        context: format!("無法改用一般身分：{reason}"),
    }
}

/// 目前程序是否以系統管理員權限執行（UAC 提權，或沒有分割 token 的系統管理員帳號）。
pub(crate) fn process_elevated() -> AppResult<bool> {
    let process = process_token()?;
    let mut elevation = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    // SAFETY: process 含 TOKEN_QUERY；輸出緩衝區是本函式存活中的區域變數，長度即其大小。
    // quality-allow: Win32 FFI，GetTokenInformation 沒有安全包裝
    unsafe {
        GetTokenInformation(
            process.get(),
            TokenElevation,
            Some((&raw mut elevation).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    }
    .map_err(|e| win_error("讀取程序的提權狀態", e))?;
    Ok(elevation.TokenIsElevated != 0)
}

/// `token` 能否修改 `install`：安裝根目錄或套用根目錄 `game\` 任一處有 [`MODIFY_RIGHTS`] 任一項即是。
/// 只看這兩層（套用只寫 `{install}\game` 之內，protocols §5.4.5；不寫 `boot\`）；個別子目錄另外放寬的權限
/// 不在判定內。讀不到 security descriptor（例如資料夾不存在）回錯誤，由呼叫端失敗關閉。
pub(crate) fn may_modify(token: &AccessToken, install: &Path) -> AppResult<bool> {
    for dir in apply_dirs(install) {
        if may_modify_dir(token, &dir)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `token` 對 `dir` 這一層是否有 [`MODIFY_RIGHTS`] 任一項（protocols §1.6：影子上層資料夾）。讀不到 security
/// descriptor 回錯誤。
pub(crate) fn may_modify_dir(token: &AccessToken, dir: &Path) -> AppResult<bool> {
    Ok(granted_access(token, dir)? & MODIFY_RIGHTS != 0)
}

fn apply_dirs(install: &Path) -> [PathBuf; 2] {
    [install.to_path_buf(), install.join("game")]
}

fn process_token() -> AppResult<Win32Handle> {
    let mut token = HANDLE::default();
    // SAFETY: GetCurrentProcess 回傳不需關閉的 pseudo handle；token 是本函式的區域變數，成功時交給
    // Win32Handle 獨占。quality-allow: Win32 FFI，OpenProcessToken 沒有安全包裝
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE, &mut token) }
        .map_err(|e| win_error("開啟程序 token", e))?;
    Ok(Win32Handle::new(token))
}

/// `token`（須含 TOKEN_QUERY）的 `class` 資訊；以 `u64` 配置確保對齊。
fn token_info(token: &Win32Handle, class: TOKEN_INFORMATION_CLASS) -> AppResult<Vec<u64>> {
    let mut len = 0u32;
    // SAFETY: 不給緩衝區只查長度（此時回 ERROR_INSUFFICIENT_BUFFER）；len 是區域變數。
    // quality-allow: Win32 FFI，GetTokenInformation 沒有安全包裝；長度查詢必定回錯，只看 len
    let _ = unsafe { GetTokenInformation(token.get(), class, None, 0, &mut len) };
    if len == 0 {
        return Err(win_error("讀取 token 資訊", windows::core::Error::from_thread()));
    }
    let mut buffer = vec![0u64; (len as usize).div_ceil(size_of::<u64>())];
    // SAFETY: buffer 至少 len bytes、呼叫期間存活。
    // quality-allow: Win32 FFI，GetTokenInformation 沒有安全包裝
    unsafe { GetTokenInformation(token.get(), class, Some(buffer.as_mut_ptr().cast()), len, &mut len) }
        .map_err(|e| win_error("讀取 token 資訊", e))?;
    Ok(buffer)
}

/// `AccessCheck` 只收 impersonation token：把 `token`（須含 TOKEN_DUPLICATE）複製成 identification 層級，
/// 新 handle 含 TOKEN_QUERY。
fn duplicate(token: &Win32Handle) -> AppResult<AccessToken> {
    let mut copy = HANDLE::default();
    // SAFETY: token 是有效且含 TOKEN_DUPLICATE 的 token handle；copy 是本函式的區域變數，成功時交給
    // Win32Handle 獨占。quality-allow: Win32 FFI，DuplicateToken 沒有安全包裝
    unsafe { DuplicateToken(token.get(), SecurityIdentification, &mut copy) }
        .map_err(|e| win_error("複製程序 token", e))?;
    Ok(AccessToken(Win32Handle::new(copy)))
}

/// `token` 對 `dir` 的有效權限（`MAXIMUM_ALLOWED`）；完全沒有權限時為 0。
fn granted_access(token: &AccessToken, dir: &Path) -> AppResult<u32> {
    let descriptor = security_descriptor(dir)?;
    let mapping = GENERIC_MAPPING {
        GenericRead: FILE_GENERIC_READ.0,
        GenericWrite: FILE_GENERIC_WRITE.0,
        GenericExecute: FILE_GENERIC_EXECUTE.0,
        GenericAll: FILE_ALL_ACCESS.0,
    };
    let mut privileges = PRIVILEGE_SET::default();
    let mut privileges_len = size_of::<PRIVILEGE_SET>() as u32;
    let mut granted = 0u32;
    let mut status = BOOL::default();
    // SAFETY: descriptor 是 GetFileSecurityW 填好、8 位元組對齊的 self-relative security descriptor，呼叫
    // 期間存活且 AccessCheck 只讀取它；token 是含 TOKEN_QUERY 的 impersonation token；其餘輸出都指向本函式
    // 存活中的區域變數，privileges_len 為 privileges 的大小。
    // quality-allow: Win32 FFI，AccessCheck 沒有安全包裝
    unsafe {
        AccessCheck(
            PSECURITY_DESCRIPTOR(descriptor.as_ptr().cast_mut().cast()),
            token.0.get(),
            MAXIMUM_ALLOWED,
            &mapping,
            Some(&mut privileges),
            &mut privileges_len,
            &mut granted,
            &mut status,
        )
    }
    .map_err(|e| win_error(&format!("判定 {} 的權限", dir.display()), e))?;
    Ok(if status.as_bool() { granted } else { 0 })
}

/// 資料夾的 self-relative security descriptor：擁有者、群組與 DACL（AccessCheck 需要），加上完整性標籤
/// （完整性規則也算進判定）。以 `u64` 配置確保對齊。
fn security_descriptor(dir: &Path) -> AppResult<Vec<u64>> {
    let path = HSTRING::from(dir);
    let info = OWNER_SECURITY_INFORMATION.0
        | GROUP_SECURITY_INFORMATION.0
        | DACL_SECURITY_INFORMATION.0
        | LABEL_SECURITY_INFORMATION.0;
    let context = || format!("讀取 {} 的權限設定", dir.display());
    let mut needed = 0u32;
    // SAFETY: path 是以 0 結尾、呼叫期間存活的 UTF-16 字串；不給緩衝區只查所需長度，needed 是區域變數。
    // quality-allow: Win32 FFI，GetFileSecurityW 沒有安全包裝
    let sized = unsafe { GetFileSecurityW(&path, info, None, 0, &mut needed) };
    // 不給緩衝區時，正常結果是回 FALSE（ERROR_INSUFFICIENT_BUFFER）並填好 needed。
    if sized.as_bool() || needed == 0 {
        return Err(win_error(&context(), windows::core::Error::from_thread()));
    }
    let mut buffer = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
    // SAFETY: buffer 至少 needed bytes、呼叫期間存活；needed 是上一次呼叫回報的長度。
    // quality-allow: Win32 FFI，GetFileSecurityW 沒有安全包裝
    unsafe {
        GetFileSecurityW(
            &path,
            info,
            Some(PSECURITY_DESCRIPTOR(buffer.as_mut_ptr().cast())),
            needed,
            &mut needed,
        )
    }
    .ok()
    .map_err(|e| win_error(&context(), e))?;
    Ok(buffer)
}

/// 測試限定：以目前程序的 token 衍生出的參考身分（代替 helper 拿到的 linked token）。
#[cfg(test)]
pub(crate) mod test_support {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        CreateWellKnownSid, PSID, SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE, TOKEN_MANDATORY_LABEL,
        WELL_KNOWN_SID_TYPE, WinAuthenticatedUserSid, WinLowLabelSid,
    };
    use windows::Win32::Storage::FileSystem::CreateDirectoryW;

    use super::*;

    /// identification 層級（同 linked token 的形態）的身分：目前程序的 token，`deny_user` 時使用者 SID、
    /// `deny_group` 選出的群組（完整性標籤除外）改成僅拒絕；`restrict_to_user` 時另以使用者 SID 當 restricting SID。
    pub(crate) fn derived(
        deny_user: bool,
        deny_group: impl Fn(&SID_AND_ATTRIBUTES) -> bool,
        restrict_to_user: bool,
    ) -> AccessToken {
        let process = process_token().expect("process token");
        let sids = TokenSids::read(&process).expect("token sids");
        let user = SID_AND_ATTRIBUTES {
            Sid: sids.user().Sid,
            Attributes: 0,
        };
        let disable: Vec<SID_AND_ATTRIBUTES> = deny_user
            .then_some(user)
            .into_iter()
            .chain(
                sids.groups()
                    .iter()
                    .filter(|g| g.Attributes & SE_GROUP_INTEGRITY as u32 == 0 && deny_group(g))
                    .map(|g| SID_AND_ATTRIBUTES { Sid: g.Sid, Attributes: 0 }),
            )
            .collect();
        let restrict = [user];
        let mut restricted = HANDLE::default();
        // SAFETY: process 含 TOKEN_DUPLICATE；disable 與 restrict 的 SID 指標指向 sids 的緩衝區，呼叫期間存活；
        // restricted 成功時交給 Win32Handle 獨占。quality-allow: Win32 FFI，CreateRestrictedToken 沒有安全包裝
        unsafe {
            CreateRestrictedToken(
                process.get(),
                DISABLE_MAX_PRIVILEGE,
                Some(&disable),
                None,
                restrict_to_user.then_some(&restrict[..]),
                &mut restricted,
            )
        }
        .expect("restricted token");
        duplicate(&Win32Handle::new(restricted)).expect("duplicate")
    }

    /// 與目前程序同權限的參考身分（UAC 開啟、以一般身分可修改安裝時，linked token 對安裝的權限）。
    pub(crate) fn own_reference() -> AccessToken {
        derived(false, |_| false, false)
    }

    /// 帶 restricting SID 的參考身分：無法縮減。
    pub(crate) fn restricted_reference() -> AccessToken {
        derived(false, |_| false, true)
    }

    /// 與目前程序同權限、但 Authenticated Users 只用於拒絕的參考身分：只授權給 Authenticated Users 的資料夾，
    /// 縮減的身分寫不進、目前程序寫得進。
    pub(crate) fn reference_without_authenticated_users() -> AccessToken {
        let authenticated_users = well_known_sid(WinAuthenticatedUserSid);
        derived(
            false,
            // SAFETY: 兩個 SID 指標都在呼叫期間存活。quality-allow: Win32 FFI，EqualSid 沒有安全包裝
            |g| unsafe { EqualSid(g.Sid, PSID(authenticated_users.as_ptr().cast_mut().cast())) }.is_ok(),
            false,
        )
    }

    /// 與目前程序同權限、但完整性等級是 Low 的參考身分（縮減第 3 步取它的完整性等級）。
    pub(crate) fn low_integrity_reference() -> AccessToken {
        let process = process_token().expect("process token");
        let mut copy = HANDLE::default();
        // SAFETY: process 含 TOKEN_DUPLICATE；copy 成功時交給 Win32Handle 獨占。
        // quality-allow: Win32 FFI，DuplicateTokenEx 沒有安全包裝
        unsafe {
            DuplicateTokenEx(
                process.get(),
                TOKEN_QUERY | TOKEN_ADJUST_DEFAULT,
                None,
                SecurityIdentification,
                TokenImpersonation,
                &mut copy,
            )
        }
        .expect("duplicate");
        let copy = Win32Handle::new(copy);
        let low = well_known_sid(WinLowLabelSid);
        let label = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: PSID(low.as_ptr().cast_mut().cast()),
                Attributes: SE_GROUP_INTEGRITY as u32,
            },
        };
        // SAFETY: label 與它指向的 low 在呼叫期間存活；copy 含 TOKEN_ADJUST_DEFAULT（降低完整性等級不需特權）。
        // quality-allow: Win32 FFI，SetTokenInformation 沒有安全包裝
        unsafe {
            SetTokenInformation(
                copy.get(),
                TokenIntegrityLevel,
                (&raw const label).cast(),
                size_of::<TOKEN_MANDATORY_LABEL>() as u32,
            )
        }
        .expect("set integrity");
        AccessToken(copy)
    }

    pub(crate) fn well_known_sid(kind: WELL_KNOWN_SID_TYPE) -> Vec<u64> {
        let mut buffer = vec![0u64; (SECURITY_MAX_SID_SIZE as usize).div_ceil(size_of::<u64>())];
        let mut size = SECURITY_MAX_SID_SIZE;
        // SAFETY: buffer 至少 SECURITY_MAX_SID_SIZE bytes、呼叫期間存活。
        // quality-allow: Win32 FFI，CreateWellKnownSid 沒有安全包裝
        unsafe { CreateWellKnownSid(kind, None, Some(PSID(buffer.as_mut_ptr().cast())), &mut size) }
            .expect("well-known sid");
        buffer
    }

    /// 以 `sddl` 的 DACL 建立資料夾。
    pub(crate) fn create_dir_with_sddl(path: &Path, sddl: &str) {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: 輸入是以 0 結尾的字串；成功時 descriptor 由系統以 LocalAlloc 配置，最後以 LocalFree 釋放。
        // quality-allow: Win32 FFI，ConvertStringSecurityDescriptorToSecurityDescriptorW 沒有安全包裝
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(sddl),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .expect("sddl");
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        // SAFETY: 路徑字串與 attributes 在呼叫期間存活；descriptor 在 LocalFree 之前有效，之後不再使用。
        // quality-allow: Win32 FFI，CreateDirectoryW 與 LocalFree 沒有安全包裝
        let created = unsafe { CreateDirectoryW(&HSTRING::from(path), Some(&attributes)) };
        // SAFETY: 同上。quality-allow: Win32 FFI
        unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
        created.expect("mkdir");
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;

    use windows::Win32::Security::{
        GetSecurityDescriptorOwner, PSID, TOKEN_MANDATORY_LABEL, TokenImpersonationLevel, WinBuiltinAdministratorsSid,
        WinLowLabelSid,
    };
    use windows::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

    use super::test_support::{
        create_dir_with_sddl, derived, low_integrity_reference, own_reference, reference_without_authenticated_users,
        restricted_reference, well_known_sid,
    };
    use super::*;

    /// tempdir 下的假安裝（根目錄與 `game\`）。
    fn install_in(root: &Path) -> PathBuf {
        let install = root.join("FINAL FANTASY XIV - A Realm Reborn");
        fs::create_dir_all(install.join("game")).expect("mkdir");
        install
    }

    /// 可修改：測試程序自己在 tempdir 建的安裝（parent 在一般資料夾的國際服安裝即屬此類）。
    #[test]
    fn own_tempdir_install_is_modifiable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let install = install_in(dir.path());
        let token = AccessToken::current().expect("token");
        assert!(may_modify(&token, &install).expect("check"));
    }

    /// 不可修改：同一份安裝，換成使用者 SID 與所有群組都只能用於拒絕的身分——DACL 沒有任何一項授予它，
    /// 它也不再算擁有者（沒有擁有者隱含的 WRITE_DAC），相當於受保護位置對一般使用者的情況。
    #[test]
    fn install_without_any_grant_is_not_modifiable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let install = install_in(dir.path());
        assert!(!may_modify(&powerless_token(), &install).expect("check"));
    }

    /// 讀不到權限設定不是「不可修改」：回錯誤，呼叫端不會因此改走提權。
    #[test]
    fn missing_folder_is_an_error_not_a_verdict() {
        let dir = tempfile::tempdir().expect("tempdir");
        let token = AccessToken::current().expect("token");
        assert!(may_modify(&token, &dir.path().join("missing")).is_err());
        // 根目錄不可修改、`game\` 不存在：查到 `game\` 時是錯誤，不當成「不可修改」。
        assert!(may_modify(&powerless_token(), dir.path()).is_err());
    }

    /// 縮減的身分不能用參考身分沒有的群組：資料夾只授權給 Authenticated Users（測試程序已啟用），參考身分把
    /// 它設成僅拒絕時，縮減後在資料夾裡建檔被拒；不模擬的同一執行緒照常能建。
    #[test]
    fn impersonation_cannot_use_groups_the_reference_lacks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let only_au = dir.path().join("au-only");
        create_dir_with_sddl(&only_au, "D:P(A;OICI;FA;;;AU)");
        fs::write(only_au.join("self.txt"), b"x").expect("目前程序經 Authenticated Users 可寫");
        let target = only_au.join("reduced.txt");
        // token handle 不能跨執行緒，參考身分在模擬的執行緒裡建立。
        let written = on_new_thread(move || {
            let _as_user = Impersonation::begin(&reference_without_authenticated_users()).expect("begin");
            fs::write(&target, b"x").map_err(|e| e.kind())
        });
        assert_eq!(written, Err(io::ErrorKind::PermissionDenied));
        assert!(!only_au.join("reduced.txt").exists());
    }

    /// 參考身分與目前程序同權限時，縮減後照常能寫：執行緒的模擬層級是 SecurityImpersonation（不是
    /// identification）、新檔擁有者是使用者；guard 結束後執行緒不再模擬。
    #[test]
    fn impersonation_with_own_reference_writes_as_user() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("reduced.txt");
        let target = file.clone();
        let (level, written, after) = on_new_thread(move || {
            let guard = Impersonation::begin(&own_reference()).expect("begin");
            let level = thread_token().map(|t| token_info(&t, TokenImpersonationLevel).expect("level")[0] as u32);
            let written = fs::write(&target, b"x").map_err(|e| e.kind());
            drop(guard);
            (level, written, thread_token().is_some())
        });
        assert_eq!(level, Some(SecurityImpersonation.0 as u32));
        assert_eq!(written, Ok(()));
        assert!(!after, "guard 結束後執行緒不再模擬");
        let process = process_token().expect("process token");
        let sids = TokenSids::read(&process).expect("sids");
        assert!(owner_is(&file, sids.user().Sid), "新檔擁有者是使用者");
    }

    /// 參考身分帶 restricting SID 時無法縮減：回錯誤，執行緒身分不變。
    #[test]
    fn impersonation_refuses_restricted_reference() {
        let (refused, impersonating) = on_new_thread(move || {
            let refused = Impersonation::begin(&restricted_reference()).is_err();
            (refused, thread_token().is_some())
        });
        assert!(refused);
        assert!(!impersonating);
    }

    /// 縮減第 3 步：完整性等級取自參考身分。參考身分是 Low 時，模擬中的執行緒 token 是 Low，寫不進目前程序
    /// （Medium）建立的暫存資料夾。
    #[test]
    fn impersonation_takes_integrity_level_from_reference() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("low.txt");
        let low = well_known_sid(WinLowLabelSid);
        let (is_low, written) = on_new_thread(move || {
            let _as_user = Impersonation::begin(&low_integrity_reference()).expect("begin");
            let token = thread_token().expect("模擬中");
            let label = token_info(&token, TokenIntegrityLevel).expect("label");
            // SAFETY: label 是 GetTokenInformation 以 TokenIntegrityLevel 填好的緩衝區，開頭是 TOKEN_MANDATORY_LABEL，
            // 它的 SID 指標指向同一緩衝區；low 在呼叫期間存活。quality-allow: Win32 FFI，EqualSid 沒有安全包裝
            let is_low = unsafe {
                EqualSid((*label.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid, PSID(low.as_ptr().cast_mut().cast()))
            }
            .is_ok();
            (is_low, fs::write(&target, b"x").map_err(|e| e.kind()))
        });
        assert!(is_low, "模擬中的執行緒 token 是 Low");
        assert_eq!(written, Err(io::ErrorKind::PermissionDenied));
    }

    /// 遊戲執行判定在沒有模擬的執行緒做：模擬中的執行緒呼叫 `unimpersonated`，裡面沒有模擬 token。
    #[test]
    fn unimpersonated_runs_without_thread_token() {
        let (inside, outside) = on_new_thread(|| {
            let _as_user = Impersonation::begin(&own_reference()).expect("begin");
            (thread_token().is_some(), unimpersonated(|| thread_token().is_some()))
        });
        assert!(inside, "模擬中的執行緒有模擬 token");
        assert!(!outside, "unimpersonated 裡沒有模擬 token");
    }

    /// 手動：須在 UAC 開啟的系統管理員帳號，以「以系統管理員身分執行」的終端機跑
    /// `node scripts/test.mjs access::tests::linked_token_reduction -- --ignored`。以真正的 linked token 縮減：
    /// 寫不進只給 Administrators 的資料夾，寫得進一般資料夾且新檔擁有者是使用者。
    #[test]
    #[ignore = "手動：需要 UAC 分割 token 的系統管理員身分"]
    fn linked_token_reduction_drops_administrators() {
        assert!(AccessToken::linked().expect("linked").is_some(), "需以 UAC 提權執行");
        let dir = tempfile::tempdir().expect("tempdir");
        let admins_only = dir.path().join("admins-only");
        create_dir_with_sddl(&admins_only, "D:P(A;OICI;FA;;;BA)");
        fs::write(admins_only.join("self.txt"), b"x").expect("提權的程序可寫");
        let plain = dir.path().join("plain.txt");
        let (target_admins, target_plain) = (admins_only.join("reduced.txt"), plain.clone());
        let (to_admins, to_plain) = on_new_thread(move || {
            let linked = AccessToken::linked().expect("linked").expect("linked token");
            let _as_user = Impersonation::begin(&linked).expect("begin");
            let to_admins = fs::write(&target_admins, b"x").map_err(|e| e.kind());
            (to_admins, fs::write(&target_plain, b"x").map_err(|e| e.kind()))
        });
        assert_eq!(to_admins, Err(io::ErrorKind::PermissionDenied));
        assert_eq!(to_plain, Ok(()));
        let admins = well_known_sid(WinBuiltinAdministratorsSid);
        assert!(!owner_is(&plain, PSID(admins.as_ptr().cast_mut().cast())), "新檔擁有者不是 Administrators");
        let process = process_token().expect("process token");
        assert!(owner_is(&plain, TokenSids::read(&process).expect("sids").user().Sid));
    }

    /// 目前程序的 token，使用者 SID 與所有群組（完整性標籤除外）都改成只用於拒絕。
    fn powerless_token() -> AccessToken {
        derived(true, |_| true, false)
    }

    /// 模擬綁在執行緒上：在新執行緒跑 `f`，不影響測試執行緒。
    fn on_new_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::spawn(f).join().expect("thread")
    }

    /// 目前執行緒的模擬 token（以程序身分開啟）；沒有模擬時為 `None`。
    fn thread_token() -> Option<Win32Handle> {
        let mut token = HANDLE::default();
        // SAFETY: GetCurrentThread 是不需關閉的 pseudo handle；token 成功時交給 Win32Handle 獨占。
        // quality-allow: Win32 FFI；沒有模擬時回 ERROR_NO_TOKEN，即 None
        unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut token) }
            .ok()
            .map(|()| Win32Handle::new(token))
    }

    /// `path` 的擁有者是否為 `sid`。
    fn owner_is(path: &Path, sid: PSID) -> bool {
        let descriptor = security_descriptor(path).expect("security descriptor");
        let mut owner = PSID::default();
        let mut defaulted = BOOL::default();
        // SAFETY: descriptor 是 GetFileSecurityW 填好的 self-relative security descriptor，owner 指向它內部，
        // 只在 descriptor 存活時使用。quality-allow: Win32 FFI，GetSecurityDescriptorOwner／EqualSid 沒有安全包裝
        unsafe {
            GetSecurityDescriptorOwner(
                PSECURITY_DESCRIPTOR(descriptor.as_ptr().cast_mut().cast()),
                &mut owner,
                &mut defaulted,
            )
            .expect("owner");
            EqualSid(owner, sid).is_ok()
        }
    }
}
