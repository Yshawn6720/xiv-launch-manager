//! 安裝資料夾的修改權判定。parent 據此決定國際服更新在程序內套用或經提權
//! helper；helper 據此拒絕寫入一般權限就能修改的安裝——那種安裝裡，一般權限的程序可以在 helper 檢查
//! junction（`zipatch::target::ensure_no_link`）與實際寫入之間把子目錄換成 junction，把系統管理員權限的
//! 寫入導到受保護的位置。
//!
//! 以 `AccessCheck` 依資料夾的 security descriptor（擁有者、群組、DACL、完整性標籤）算出指定身分的有效
//! 權限，不在遊戲目錄建立或刪除任何檔案；parent 與 helper 用同一個判定，結論一致。本程式只發 x64，64 位元
//! 程序沒有 UAC 檔案虛擬化，判定與實際寫入的結果相同。

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::{
    AccessCheck, DACL_SECURITY_INFORMATION, DuplicateToken, GENERIC_MAPPING,
    GROUP_SECURITY_INFORMATION, GetFileSecurityW, GetTokenInformation, LABEL_SECURITY_INFORMATION,
    OWNER_SECURITY_INFORMATION, PRIVILEGE_SET, PSECURITY_DESCRIPTOR, SecurityIdentification,
    TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_ELEVATION_TYPE, TOKEN_LINKED_TOKEN, TOKEN_QUERY, TokenElevation,
    TokenElevationType, TokenElevationTypeFull, TokenLinkedToken,
};
use windows::Win32::Storage::FileSystem::{
    DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ALL_ACCESS, FILE_DELETE_CHILD,
    FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, WRITE_DAC, WRITE_OWNER,
};
use windows::Win32::System::SystemServices::MAXIMUM_ALLOWED;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
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

    /// 啟動 helper 的一般權限身分。helper 經 UAC 同意提權時，程序 token 是 UAC 分出的完整 token，取它連結的
    /// 受限 token（`TokenLinkedToken`，同一次登入未提權的身分）；不是這種 token（沒有提權、UAC 關閉、帳號
    /// 沒有分割 token）時用目前程序自己的身分。
    pub(crate) fn unelevated() -> AppResult<Self> {
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
            return duplicate(&process);
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
        // 可以直接交給 AccessCheck。
        Ok(Self(Win32Handle::new(linked.LinkedToken)))
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

#[cfg(test)]
mod tests {
    use std::fs;

    use windows::Win32::Security::{
        CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, SID_AND_ATTRIBUTES, TOKEN_GROUPS,
        TOKEN_INFORMATION_CLASS, TOKEN_USER, TokenGroups, TokenUser,
    };
    use windows::Win32::System::SystemServices::SE_GROUP_INTEGRITY;

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

    /// 目前程序的 token，使用者 SID 與所有群組（完整性標籤除外）都改成只用於拒絕。
    fn powerless_token() -> AccessToken {
        let process = process_token().expect("process token");
        let user_buf = token_info(&process, TokenUser);
        let groups_buf = token_info(&process, TokenGroups);
        // SAFETY: 兩個緩衝區由 GetTokenInformation 依各自的資訊類別填好、8 位元組對齊，在本函式結束前存活；
        // 群組陣列接在 TOKEN_GROUPS 之後共 GroupCount 項，指標由整個緩衝區導出，SID 指標也指向同一緩衝區。
        // quality-allow: 測試以 Win32 token 結構建立受限身分，沒有安全包裝
        let (user, groups) = unsafe {
            let user = (*user_buf.as_ptr().cast::<TOKEN_USER>()).User;
            let header = groups_buf.as_ptr().cast::<TOKEN_GROUPS>();
            let first = (&raw const (*header).Groups).cast::<SID_AND_ATTRIBUTES>();
            (user, std::slice::from_raw_parts(first, (*header).GroupCount as usize))
        };
        let disable: Vec<SID_AND_ATTRIBUTES> = std::iter::once(user)
            .chain(
                groups
                    .iter()
                    .copied()
                    .filter(|g| g.Attributes & SE_GROUP_INTEGRITY as u32 == 0),
            )
            .map(|g| SID_AND_ATTRIBUTES {
                Sid: g.Sid,
                Attributes: 0,
            })
            .collect();
        let mut restricted = HANDLE::default();
        // SAFETY: process 含 TOKEN_DUPLICATE；disable 的 SID 指標在呼叫期間存活；restricted 成功時交給
        // Win32Handle 獨占。quality-allow: Win32 FFI，CreateRestrictedToken 沒有安全包裝
        unsafe {
            CreateRestrictedToken(
                process.get(),
                DISABLE_MAX_PRIVILEGE,
                Some(&disable),
                None,
                None,
                &mut restricted,
            )
        }
        .expect("restricted token");
        duplicate(&Win32Handle::new(restricted)).expect("duplicate")
    }

    fn token_info(token: &Win32Handle, class: TOKEN_INFORMATION_CLASS) -> Vec<u64> {
        let mut len = 0u32;
        // SAFETY: 不給緩衝區只查長度（此時回 ERROR_INSUFFICIENT_BUFFER）；len 是區域變數。
        // quality-allow: Win32 FFI，GetTokenInformation 沒有安全包裝
        unsafe { GetTokenInformation(token.get(), class, None, 0, &mut len) }
            .expect_err("只查長度");
        let mut buffer = vec![0u64; (len as usize).div_ceil(size_of::<u64>())];
        // SAFETY: buffer 至少 len bytes、呼叫期間存活。
        // quality-allow: Win32 FFI，GetTokenInformation 沒有安全包裝
        unsafe { GetTokenInformation(token.get(), class, Some(buffer.as_mut_ptr().cast()), len, &mut len) }
            .expect("token info");
        buffer
    }
}
