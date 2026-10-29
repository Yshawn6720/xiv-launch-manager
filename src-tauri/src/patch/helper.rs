//! 國際服提權 helper（protocols §1.6「重建 helper」）。
//!
//! 同一個 exe 以 `--apply-patches <job 絕對路徑> <job SHA-256>`（套用 patch）或
//! `--build-shadow <安裝根目錄> <外部包資料夾>`（重建影子資料夾）經 UAC 啟動。`main` 第一件事就呼叫
//! [`dispatch_from_args`]，在 `run()` 與 single-instance plugin 之前分派：helper 不建 Tauri、
//! WebView、updater，只做「核對＋套用＋寫 `.ver`、`.bck`」或「核對＋重建影子」，以 exit code 結束，不輸出任何內容。
//! parent 只以 exit code 判定成敗；進度檔僅供顯示。
//!
//! exit code：0＝成功、10＝寫入前中止（job 或進度檔無效、安裝路徑不符、遊戲執行中、無法判定安裝資料夾的
//! 權限、核對或掃描失敗、之前的 patch 已完整套用而下一個在寫入前失敗；影子：動到影子之前中止）、11＝另一個
//! 套用佔著同一安裝的 named mutex、12＝安裝資料夾（影子：影子上層資料夾）一般權限就能修改（拒絕以系統管理員
//! 權限寫入）、20＝已寫入後失敗、2＝參數不符。

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::iter;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{ERROR_CANCELLED, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY, FILE_NAME_NORMALIZED,
    FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, GETFINALPATHNAMEBYHANDLE_FLAGS,
    GetFileInformationByHandle, GetFinalPathNameByHandleW, VOLUME_NAME_DOS, VOLUME_NAME_NT,
};
use windows::Win32::System::Threading::{
    CreateMutexW, GetExitCodeProcess, INFINITE, ReleaseMutex, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{
    SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::{HRESULT, PCWSTR, w};

use crate::error::{AppError, AppResult};
use crate::game;
use crate::global::shadow;
use crate::patch::access::{self, AccessToken};
use crate::patch::apply::{ApplyFailure, ApplyProgress, apply_patches};
use crate::patch::job::{self, Job, is_plain_absolute, is_sha256_hex, read_job, sha256_hex};
use crate::patch::running::{Win32Handle, comparable, exe_running, install_in_use, normalize_path, win_error};
use crate::region::Region;

const FLAG: &str = "--apply-patches";
const SHADOW_FLAG: &str = "--build-shadow";
const EXIT_SUCCESS: i32 = 0;
const EXIT_BAD_ARGS: i32 = 2;
const EXIT_ABORTED_BEFORE_WRITE: i32 = 10;
const EXIT_BUSY: i32 = 11;
const EXIT_INSTALL_MODIFIABLE: i32 = 12;
const EXIT_FAILED_AFTER_WRITE: i32 = 20;
const MUTEX_PREFIX: &str = r"Local\xiv-launch-manager-patch-";

/// parent 看到的 helper 結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperExit {
    /// 0：全部 patch 套用並寫好 `.ver`。
    Success,
    /// 10：失敗的 patch 未寫入任何遊戲檔，之前的都已完整套用（遊戲目錄處於某個完整版本），可清除
    /// dirty 標記。
    AbortedBeforeWrite,
    /// 11：另一個套用正持有同一安裝的 named mutex（可能正在寫），保留 dirty 標記。
    Busy,
    /// 12：啟動 helper 的一般權限身分就能修改安裝根目錄或 `game\`，helper 沒有寫入任何遊戲檔就結束
    /// （[`access::may_modify`]）。可清除 dirty 標記。
    InstallModifiable,
    /// 20：已開始寫入遊戲目錄後失敗，保留 dirty 標記。
    FailedAfterWrite,
    /// 使用者在 UAC 拒絕，helper 沒有啟動。
    UacDeclined,
    /// 確定 helper 沒有執行：參數無效、取不到程式路徑、`ShellExecuteExW` 以 `ERROR_CANCELLED` 以外的
    /// 錯誤失敗，或沒有回傳程序 handle。遊戲目錄未被寫過。
    NotStarted,
    /// 其他 exit code（含 2＝參數不符、程序被結束或崩潰），保留 dirty 標記。
    Other(i32),
}

impl HelperExit {
    fn from_exit_code(code: i32) -> Self {
        match code {
            EXIT_SUCCESS => Self::Success,
            EXIT_ABORTED_BEFORE_WRITE => Self::AbortedBeforeWrite,
            EXIT_BUSY => Self::Busy,
            EXIT_INSTALL_MODIFIABLE => Self::InstallModifiable,
            EXIT_FAILED_AFTER_WRITE => Self::FailedAfterWrite,
            other => Self::Other(other),
        }
    }
}

/// `main` 第一行呼叫：命令列不含 `--apply-patches`、`--build-shadow` 回 `None`（照常 `run()`）；含時執行
/// helper 並回傳 exit code（`main` 以 `std::process::exit` 結束），不建 Tauri。旗標出現但形式不符一律回 2，
/// 不會退回啟動 GUI（避免提權程序長成完整介面）。
pub fn dispatch_from_args() -> Option<i32> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match parse_args(&args) {
        Invocation::Normal => None,
        Invocation::Invalid => Some(EXIT_BAD_ARGS),
        Invocation::Apply { job, sha256 } => Some(run_helper(&job, &sha256)),
        Invocation::Shadow { install, pack } => Some(run_shadow_helper(&install, &pack)),
    }
}

/// parent 用：以 `ShellExecuteExW`（verb `runas`）啟動同一 exe `--apply-patches <job> <sha256>`，等它
/// 結束後依 exit code 回傳結果；UAC 被拒（`ERROR_CANCELLED`）回 [`HelperExit::UacDeclined`]，其他
/// 沒能啟動的情況回 [`HelperExit::NotStarted`]。只有程序已啟動、但等待或讀取 exit code 失敗才回
/// `Err`（helper 可能仍在執行或已寫入）。阻塞到 helper 結束（含 UAC 對話框），呼叫端以
/// `spawn_blocking` 包。
pub fn run_elevated(job_path: &Path, job_sha256: &str) -> AppResult<HelperExit> {
    if !job_path.is_absolute() || !is_sha256_hex(job_sha256) {
        return Ok(HelperExit::NotStarted);
    }
    shell_execute_elevated(&helper_parameters(job_path, job_sha256), "更新程式")
}

/// [`run_elevated`] 的影子版：啟動 `--build-shadow <install> <pack>`（protocols §1.6「重建 helper」）。
/// 兩個路徑都必須是一般的絕對路徑，否則不啟動。
pub fn run_shadow_elevated(install: &Path, pack: &Path) -> AppResult<HelperExit> {
    if !is_plain_absolute(install) || !is_plain_absolute(pack) {
        return Ok(HelperExit::NotStarted);
    }
    shell_execute_elevated(&shadow_parameters(install, pack), "影子建立程式")
}

/// 以 verb `runas` 啟動同一 exe、帶 `parameters`（以 0 結尾的 UTF-16），等它結束並轉換 exit code；`name`
/// 只用於錯誤訊息。結果的意義見 [`run_elevated`]。
fn shell_execute_elevated(parameters: &[u16], name: &str) -> AppResult<HelperExit> {
    let Ok(exe) = std::env::current_exe() else {
        return Ok(HelperExit::NotStarted);
    };
    let file = wide_nul(exe.as_os_str());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        // NOASYNC：呼叫端是沒有訊息迴圈的背景執行緒，要求呼叫返回前完成啟動。
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: info 的字串指標指向以 0 結尾且呼叫期間存活的緩衝區（file 由本函式持有，parameters 由呼叫端
    // 借出）；cbSize 已設定。成功時 hProcess 交給 Win32Handle 獨占。
    // quality-allow: Win32 FFI，ShellExecuteExW 沒有安全包裝
    match unsafe { ShellExecuteExW(&mut info) } {
        Ok(()) => {}
        Err(e) if e.code() == HRESULT::from_win32(ERROR_CANCELLED.0) => {
            return Ok(HelperExit::UacDeclined);
        }
        Err(_) => return Ok(HelperExit::NotStarted),
    }
    if info.hProcess.is_invalid() {
        return Ok(HelperExit::NotStarted);
    }
    let process = Win32Handle::new(info.hProcess);
    // SAFETY: process 是 ShellExecuteExW 以 SEE_MASK_NOCLOSEPROCESS 回傳的有效程序 handle。
    // quality-allow: Win32 FFI，WaitForSingleObject 沒有安全包裝
    if unsafe { WaitForSingleObject(process.get(), INFINITE) } != WAIT_OBJECT_0 {
        return Err(AppError::Install {
            context: format!("等待{name}結束失敗"),
        });
    }
    let mut code = 0u32;
    // SAFETY: process 是有效的程序 handle；code 是本函式存活中的區域變數。
    // quality-allow: Win32 FFI，GetExitCodeProcess 沒有安全包裝
    unsafe { GetExitCodeProcess(process.get(), &mut code) }
        .map_err(|e| win_error(&format!("讀取{name}結果"), e))?;
    // exit code 是 32 位元值；崩潰的 NTSTATUS（0xC…）轉成負數，落在 Other。
    Ok(HelperExit::from_exit_code(code as i32))
}

#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    Normal,
    Invalid,
    Apply { job: PathBuf, sha256: String },
    Shadow { install: PathBuf, pack: PathBuf },
}

/// `args` 不含程式名稱。只認恰好 `--apply-patches <job 絕對路徑> <64 位小寫 hex>`，或
/// `--build-shadow <安裝根目錄> <外部包資料夾>`（兩者都是一般的絕對路徑）；兩個旗標都沒出現才是一般啟動。
fn parse_args(args: &[OsString]) -> Invocation {
    if !args.iter().any(|arg| arg == FLAG || arg == SHADOW_FLAG) {
        return Invocation::Normal;
    }
    let [flag, first, second] = args else {
        return Invocation::Invalid;
    };
    if flag == FLAG {
        let job = PathBuf::from(first);
        return match second.to_str() {
            Some(sha256) if job.is_absolute() && is_sha256_hex(sha256) => Invocation::Apply {
                job,
                sha256: sha256.to_string(),
            },
            _ => Invocation::Invalid,
        };
    }
    let (install, pack) = (PathBuf::from(first), PathBuf::from(second));
    if flag == SHADOW_FLAG && is_plain_absolute(&install) && is_plain_absolute(&pack) {
        Invocation::Shadow { install, pack }
    } else {
        Invocation::Invalid
    }
}

/// `--apply-patches "<job>" <sha256>`（以 0 結尾的 UTF-16）。
fn helper_parameters(job_path: &Path, sha256: &str) -> Vec<u16> {
    let mut out: Vec<u16> = FLAG.encode_utf16().collect();
    out.push(u16::from(b' '));
    quote_arg(job_path.as_os_str(), &mut out);
    out.push(u16::from(b' '));
    out.extend(sha256.encode_utf16());
    out.push(0);
    out
}

/// `--build-shadow "<install>" "<pack>"`（以 0 結尾的 UTF-16）。
fn shadow_parameters(install: &Path, pack: &Path) -> Vec<u16> {
    let mut out: Vec<u16> = SHADOW_FLAG.encode_utf16().collect();
    for path in [install, pack] {
        out.push(u16::from(b' '));
        quote_arg(path.as_os_str(), &mut out);
    }
    out.push(0);
    out
}

/// 依 Windows 命令列切分規則（`CommandLineToArgvW`，即 Rust `args_os` 的解析方式）把一個參數包成
/// `"…"`：`"` 前的反斜線加倍並跳脫 `"`，結尾的反斜線加倍（避免跳脫收尾引號），其他反斜線原樣。
fn quote_arg(arg: &OsStr, out: &mut Vec<u16>) {
    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    out.push(QUOTE);
    let mut backslashes = 0;
    for unit in arg.encode_wide() {
        match unit {
            BACKSLASH => backslashes += 1,
            QUOTE => {
                out.extend(iter::repeat_n(BACKSLASH, backslashes + 1));
                backslashes = 0;
            }
            _ => backslashes = 0,
        }
        out.push(unit);
    }
    out.extend(iter::repeat_n(BACKSLASH, backslashes));
    out.push(QUOTE);
}

fn wide_nul(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(iter::once(0)).collect()
}

/// helper 本體。任何錯誤都只轉成 exit code，不輸出內容。寫入目標一律用 HKLM 解析出的安裝根目錄，
/// job 的 `expected_install` 只用來交叉核對。安裝資料夾一般權限就能修改時拒絕寫入：那種安裝裡一般權限程序
/// 可以在 junction 檢查與實際寫入之間換掉子目錄，把系統管理員權限的寫入導到別處（parent 以一般身分執行時遇到
/// 可修改的安裝本來就在程序內套用；以系統管理員身分執行時一律啟動 helper，由這裡判定）。
fn run_helper(job_path: &Path, sha256: &str) -> i32 {
    let Ok((job, mut progress)) = open_job(job_path, sha256) else {
        return EXIT_ABORTED_BEFORE_WRITE;
    };
    let Ok(expected) = normalize_path(&job.expected_install) else {
        return EXIT_ABORTED_BEFORE_WRITE;
    };
    let Some(_lock) = InstallLock::acquire(&expected) else {
        return EXIT_BUSY;
    };
    let Some(install) = game::detect_install(Region::Global) else {
        return EXIT_ABORTED_BEFORE_WRITE;
    };
    if !matches!(normalize_path(&install), Ok(actual) if actual == expected) {
        return EXIT_ABORTED_BEFORE_WRITE;
    }
    match AccessToken::unelevated().and_then(|token| access::may_modify(&token, &install)) {
        Ok(false) => {}
        Ok(true) => return EXIT_INSTALL_MODIFIABLE,
        Err(_) => return EXIT_ABORTED_BEFORE_WRITE,
    }
    if !matches!(install_in_use(&install), Ok(false)) {
        return EXIT_ABORTED_BEFORE_WRITE;
    }
    match apply_patches(Region::Global, &install, &job.patches, &mut |p| progress.write(p), &|| false) {
        Ok(()) => EXIT_SUCCESS,
        Err(ApplyFailure::BeforeWrite(_)) => EXIT_ABORTED_BEFORE_WRITE,
        Err(ApplyFailure::AfterWrite(_)) => EXIT_FAILED_AFTER_WRITE,
    }
}

/// 影子 helper 本體（protocols §1.6「重建 helper」）。任何錯誤都只轉成 exit code。安裝路徑以 HKLM 為準，命令列的
/// 只用來核對；外部包在這裡重新檢查位置與檔名。影子上層資料夾一般權限就能修改時拒絕：那種位置的一般程序
/// 可以在建立途中把影子裡的資料夾換成連結，把系統管理員權限的建立導到別處（parent 遇到這種位置本來就在
/// 程序內重建，不會啟動 helper）。影子根目錄以受保護的 DACL 建立，裡面的資料夾不受上層權限影響。
fn run_shadow_helper(install_arg: &Path, pack_dir: &Path) -> i32 {
    let Ok(expected) = normalize_path(install_arg) else {
        return EXIT_ABORTED_BEFORE_WRITE;
    };
    let Some(_lock) = InstallLock::acquire(&expected) else {
        return EXIT_BUSY;
    };
    let Some(install) = game::detect_install(Region::Global) else {
        return EXIT_ABORTED_BEFORE_WRITE;
    };
    if !matches!(normalize_path(&install), Ok(actual) if actual == expected) {
        return EXIT_ABORTED_BEFORE_WRITE;
    }
    let Some(root) = shadow::root_for(&install) else {
        return EXIT_ABORTED_BEFORE_WRITE;
    };
    let Some(parent) = root.parent() else {
        return EXIT_ABORTED_BEFORE_WRITE;
    };
    // 持有到 build 結束：這段期間上層與它的每一層上層都不能被改名或換成連結。
    let Ok(_parent_dir) = hold_shadow_parent(parent) else {
        return EXIT_ABORTED_BEFORE_WRITE;
    };
    let Ok(token) = AccessToken::unelevated() else {
        return EXIT_ABORTED_BEFORE_WRITE;
    };
    match access::may_modify_dir(&token, parent) {
        Ok(false) => {}
        Ok(true) => return EXIT_INSTALL_MODIFIABLE,
        Err(_) => return EXIT_ABORTED_BEFORE_WRITE,
    }
    // 影子的遊戲不論屬於哪個安裝，在執行就不重建（重建會刪掉它正在用的檔案）。
    let shadow_exe = shadow::game_exe(&root);
    let shadow_running = shadow_exe.is_file() && !matches!(exe_running(&shadow_exe), Ok(false));
    if shadow_running || !matches!(install_in_use(&install), Ok(false)) {
        return EXIT_ABORTED_BEFORE_WRITE;
    }
    let Ok(pack) = shadow::check_dir(pack_dir, Some(&install)) else {
        return EXIT_ABORTED_BEFORE_WRITE;
    };
    let Ok(plan) = shadow::plan(&install, &pack) else {
        return EXIT_ABORTED_BEFORE_WRITE;
    };
    match shadow::build(&root, &install, &plan, shadow::create_protected_dir) {
        Ok(()) => EXIT_SUCCESS,
        Err(_) => EXIT_FAILED_AFTER_WRITE,
    }
}

/// 讀命令列的 job 並開啟它的進度檔。進度檔不由 job 指定，固定是
/// [`job::progress_path`]（job 所在目錄下）。三個 handle 都開著時以 `GetFinalPathNameByHandleW` 取最終
/// 路徑核對：job 目錄以不跟隨最後一層 reparse point 開啟，`job.json` 與進度檔的父目錄都必須等於它。
/// job 目錄本身是 junction／symlink（例如在 UAC 對話框期間被換掉）、`job.json` 或進度檔經連結落在
/// 別處，都在任何寫入之前回錯；handle 隨之關閉。路徑任一成分是 DOS 裝置名也在開檔前拒絕。
fn open_job(job_path: &Path, sha256: &str) -> AppResult<(Job, ProgressFile)> {
    let job_dir = match job_path.parent() {
        Some(dir) if is_plain_absolute(job_path) => dir,
        _ => return Err(untrusted("job 路徑不是一般的絕對路徑")),
    };
    let dir = open_job_dir(job_dir)?;
    let (job, job_file) = read_job(job_path, sha256)?;
    let progress = ProgressFile::open(&job::progress_path(job_dir))?;
    let dir_final = final_path(&dir, VOLUME_NAME_NT)?;
    for file in [&job_file, &progress.0] {
        if final_path(file, VOLUME_NAME_NT)?.parent() != Some(dir_final.as_path()) {
            return Err(untrusted("job 或進度檔不在命令列的 job 目錄內"));
        }
    }
    Ok((job, progress))
}

/// 開啟 job 目錄本身（不跟隨最後一層 reparse point），必須是一般目錄；只要求讀屬性權限。
fn open_job_dir(dir: &Path) -> AppResult<File> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(dir)
        .map_err(|e| AppError::io("開啟 job 目錄", e))?;
    let attributes = file
        .metadata()
        .map_err(|e| AppError::io("讀取 job 目錄", e))?
        .file_attributes();
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || attributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
    {
        return Err(untrusted("job 目錄是連結或不是目錄"));
    }
    Ok(file)
}

/// 開啟影子上層資料夾，共用模式不含刪除：持有期間它與它的每一層上層都不能被改名或刪除（NTFS 拒絕改名
/// 底下有開啟中項目的資料夾），之後以字面路徑做的檢查與建立都落在這個資料夾。要求列出內容的權限：只要求
/// 讀屬性的 handle 不參與共用模式檢查，擋不住改名。它本身是連結、不是目錄，或最終路徑和字面路徑不同
/// （某一層上層是 junction／symlink，連結的目標可以被改掉）都回錯。
fn hold_shadow_parent(parent: &Path) -> AppResult<File> {
    let file = OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY.0 | FILE_READ_ATTRIBUTES.0)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(parent)
        .map_err(|e| AppError::io("開啟影子資料夾的上層", e))?;
    let attributes = file
        .metadata()
        .map_err(|e| AppError::io("讀取影子資料夾的上層", e))?
        .file_attributes();
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 || attributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
        return Err(untrusted("影子資料夾的上層是連結或不是目錄"));
    }
    if comparable(&final_path(&file, VOLUME_NAME_DOS)?) != comparable(parent) {
        return Err(untrusted("影子資料夾的上層路徑經過連結"));
    }
    Ok(file)
}

/// handle 的最終路徑（`GetFinalPathNameByHandleW`：展開 junction／symlink、長檔名與實際大小寫）。`volume`
/// 選磁碟區的寫法：`VOLUME_NAME_NT` 是 NT 裝置路徑，不受磁碟代號對應影響；`VOLUME_NAME_DOS` 是 `\\?\C:\…`。
fn final_path(file: &File, volume: GETFINALPATHNAMEBYHANDLE_FLAGS) -> AppResult<PathBuf> {
    let handle = HANDLE(file.as_raw_handle());
    let flags = GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | volume.0);
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: handle 由 file 持有、呼叫期間有效；buf 是本函式持有的可寫緩衝區。
        // quality-allow: Win32 FFI，GetFinalPathNameByHandleW 沒有安全包裝
        let len = unsafe { GetFinalPathNameByHandleW(handle, &mut buf, flags) } as usize;
        if len == 0 {
            return Err(AppError::io("解析最終路徑", std::io::Error::last_os_error()));
        }
        // 放得下時回傳不含結尾 0 的長度；放不下時回傳所需大小（含結尾 0）。
        if len < buf.len() {
            buf.truncate(len);
            return Ok(PathBuf::from(OsString::from_wide(&buf)));
        }
        buf.resize(len, 0);
    }
}

fn untrusted(reason: &str) -> AppError {
    AppError::Install {
        context: format!("更新程式拒絕：{reason}"),
    }
}

/// parent 預先建立的進度檔。helper 以提權身分寫入，所以只接受「已存在、空的一般檔案、只有一個
/// 名稱」：最後一層不跟隨 reparse point、不建立新檔、有內容即拒絕、硬連結（另一個名稱可能在 job 目錄
/// 外）即拒絕，偽造的 job 無法藉此截斷或覆寫任意既有檔案。位置由 [`open_job`] 核對。
struct ProgressFile(File);

impl ProgressFile {
    fn open(path: &Path) -> AppResult<Self> {
        let file = OpenOptions::new()
            .write(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)
            .map_err(|e| AppError::io("開啟進度檔", e))?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: handle 由 file 持有、呼叫期間有效；info 是本函式存活中的區域變數。
        // quality-allow: Win32 FFI，GetFileInformationByHandle 沒有安全包裝
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
            .map_err(|e| win_error("讀取進度檔", e))?;
        let not_plain = FILE_ATTRIBUTE_REPARSE_POINT.0 | FILE_ATTRIBUTE_DIRECTORY.0;
        let empty = info.nFileSizeHigh == 0 && info.nFileSizeLow == 0;
        if info.dwFileAttributes & not_plain != 0 || !empty || info.nNumberOfLinks != 1 {
            return Err(untrusted("進度檔不是 parent 建立的空檔"));
        }
        Ok(Self(file))
    }

    /// 以一行 JSON 覆寫整個檔案。進度只供顯示：寫入失敗不影響套用，也不回報。
    fn write(&mut self, progress: ApplyProgress) {
        let Ok(mut line) = serde_json::to_vec(&progress) else {
            return;
        };
        line.push(b'\n');
        let file = &mut self.0;
        // quality-allow: 進度只影響畫面，寫不進去不影響套用本身
        let _ = file
            .seek(SeekFrom::Start(0))
            .and_then(|_| file.write_all(&line))
            .and_then(|()| file.set_len(line.len() as u64));
    }
}

/// 以安裝根目錄（[`normalize_path`] 形式）的 SHA-256 命名的跨程序 named mutex；Drop 時釋放。
struct InstallLock(Win32Handle);

impl InstallLock {
    /// 取不到（另一個程序持有，或建立失敗）回 `None`。前一個持有者異常結束（abandoned）也算取得：
    /// 寫到一半的情況由 parent 的 dirty 標記處理。
    fn acquire(normalized_install: &str) -> Option<Self> {
        let digest = sha256_hex(normalized_install.as_bytes()).ok()?;
        let name = wide_nul(OsStr::new(&format!("{MUTEX_PREFIX}{digest}")));
        // SAFETY: name 是以 0 結尾、呼叫期間存活的 UTF-16 字串；成功時回傳的 handle 交給 Win32Handle 獨占。
        // quality-allow: Win32 FFI，CreateMutexW 沒有安全包裝
        let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }.ok()?;
        let handle = Win32Handle::new(handle);
        // SAFETY: handle 是有效的 mutex handle；逾時 0 不會阻塞。
        // quality-allow: Win32 FFI，WaitForSingleObject 沒有安全包裝
        let state = unsafe { WaitForSingleObject(handle.get(), 0) };
        (state == WAIT_OBJECT_0 || state == WAIT_ABANDONED).then_some(Self(handle))
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        // SAFETY: 本執行緒在 acquire 取得這個 mutex，釋放一次；handle 之後由 Win32Handle 關閉。
        // quality-allow: Win32 FFI；釋放失敗時程序結束也會放掉 mutex，Drop 無法回傳錯誤
        let _ = unsafe { ReleaseMutex(self.0.get()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::job::{JobPatch, write_job};
    use crate::patch::list::Repo;
    use crate::patch::update::test_support::{EX1_VERSION, OLD_VERSION, TW_EX1_PATCH, snapshot};
    use crate::patch::zipatch::test_support::junction;

    const SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn parse_args_without_flag_is_normal_start() {
        assert_eq!(parse_args(&[]), Invocation::Normal);
        assert_eq!(parse_args(&args(&["--minimized"])), Invocation::Normal);
    }

    #[test]
    fn parse_args_accepts_exact_form() {
        assert_eq!(
            parse_args(&args(&[FLAG, r"C:\Users\a b\job.json", SHA])),
            Invocation::Apply {
                job: PathBuf::from(r"C:\Users\a b\job.json"),
                sha256: SHA.into(),
            }
        );
    }

    #[test]
    fn parse_args_rejects_malformed_helper_invocation() {
        let upper = SHA.to_uppercase();
        let short = &SHA[..63];
        let not_hex = "g".repeat(64);
        for bad in [
            vec![FLAG],
            vec![FLAG, r"C:\job.json"],
            vec![FLAG, r"C:\job.json", SHA, "extra"],
            vec![r"C:\job.json", FLAG, SHA],
            vec![FLAG, "job.json", SHA],
            vec![FLAG, r"\job.json", SHA],
            vec![FLAG, r"C:\job.json", upper.as_str()],
            vec![FLAG, r"C:\job.json", short],
            vec![FLAG, r"C:\job.json", not_hex.as_str()],
        ] {
            assert_eq!(parse_args(&args(&bad)), Invocation::Invalid, "{bad:?}");
        }
    }

    fn quoted(arg: &str) -> String {
        let mut out = Vec::new();
        quote_arg(OsStr::new(arg), &mut out);
        String::from_utf16(&out).unwrap()
    }

    #[test]
    fn quote_arg_follows_command_line_rules() {
        assert_eq!(quoted(r"C:\Users\a b\job.json"), r#""C:\Users\a b\job.json""#);
        assert_eq!(quoted(r"C:\dir\"), r#""C:\dir\\""#);
        assert_eq!(quoted(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quoted(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(quoted(""), r#""""#);
    }

    #[test]
    fn helper_parameters_layout() {
        let params = helper_parameters(Path::new(r"C:\x y\job.json"), SHA);
        assert_eq!(params.last(), Some(&0));
        let text = String::from_utf16(&params[..params.len() - 1]).unwrap();
        assert_eq!(text, format!(r#"{FLAG} "C:\x y\job.json" {SHA}"#));
    }

    #[test]
    fn exit_code_mapping() {
        assert_eq!(HelperExit::from_exit_code(0), HelperExit::Success);
        assert_eq!(HelperExit::from_exit_code(10), HelperExit::AbortedBeforeWrite);
        assert_eq!(HelperExit::from_exit_code(11), HelperExit::Busy);
        assert_eq!(HelperExit::from_exit_code(12), HelperExit::InstallModifiable);
        assert_eq!(HelperExit::from_exit_code(20), HelperExit::FailedAfterWrite);
        assert_eq!(HelperExit::from_exit_code(2), HelperExit::Other(2));
        assert_eq!(
            HelperExit::from_exit_code(-1_073_741_819),
            HelperExit::Other(-1_073_741_819)
        );
    }

    #[test]
    fn progress_file_must_exist_and_be_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.json");
        assert!(ProgressFile::open(&missing).is_err());
        assert!(!missing.exists(), "不得建立新檔");

        let filled = dir.path().join("filled.txt");
        std::fs::write(&filled, b"keep").unwrap();
        assert!(ProgressFile::open(&filled).is_err());
        assert_eq!(std::fs::read(filled).unwrap(), b"keep");

        assert!(ProgressFile::open(dir.path()).is_err());
    }

    #[test]
    fn progress_file_is_overwritten_with_one_json_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.json");
        std::fs::write(&path, b"").unwrap();
        let mut file = ProgressFile::open(&path).unwrap();
        file.write(ApplyProgress {
            patch_index: 0,
            patch_count: 2,
            done: 123_456,
            total: 999_999,
        });
        let last = ApplyProgress {
            patch_index: 1,
            patch_count: 2,
            done: 1,
            total: 2,
        };
        file.write(last);
        let text = std::fs::read_to_string(path).unwrap();
        assert_eq!(text, "{\"patchIndex\":1,\"patchCount\":2,\"done\":1,\"total\":2}\n");
        assert_eq!(serde_json::from_str::<ApplyProgress>(&text).unwrap(), last);
    }

    /// `GetFinalPathNameByHandleW` 的最終路徑會展開路徑中間的 junction，所以經連結開到別處的檔案，父目錄
    /// 與 job 目錄 handle 的最終路徑不同；job 目錄本身是 junction 時，不跟隨開啟就被拒。
    #[test]
    fn final_path_exposes_junction_detours() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        for d in [&a, &b] {
            std::fs::create_dir(d).unwrap();
            std::fs::write(d.join("x.json"), b"").unwrap();
        }
        junction(&a.join("link"), &b);
        let a_final = final_path(&open_job_dir(&a).unwrap(), VOLUME_NAME_NT).unwrap();
        let direct = File::open(a.join("x.json")).unwrap();
        let detour = File::open(a.join(r"link\x.json")).unwrap();
        assert_eq!(final_path(&direct, VOLUME_NAME_NT).unwrap().parent(), Some(a_final.as_path()));
        assert_ne!(final_path(&detour, VOLUME_NAME_NT).unwrap().parent(), Some(a_final.as_path()));
        assert!(open_job_dir(&a.join("link")).is_err());
        assert!(open_job_dir(&a.join("x.json")).is_err(), "不是目錄");
    }

    /// 影子上層持有期間，它和它的上層都不能改名；本身是 junction、或路徑中間經過 junction 都拒絕。
    #[test]
    fn shadow_parent_is_pinned_and_must_not_pass_through_links() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let base = PathBuf::from(comparable(&base));
        let parent = base.join(r"gp\parent");
        std::fs::create_dir_all(&parent).unwrap();
        let held = hold_shadow_parent(&parent).expect("一般資料夾");
        assert!(std::fs::rename(&parent, base.join(r"gp\moved")).is_err(), "上層不能改名");
        assert!(std::fs::rename(base.join("gp"), base.join("gp2")).is_err(), "再上一層也不能改名");
        drop(held);
        std::fs::rename(base.join("gp"), base.join("gp2")).expect("放開後可以改名");

        junction(&base.join("gp"), &base.join("gp2"));
        assert!(hold_shadow_parent(&base.join("gp")).is_err(), "上層本身是 junction");
        assert!(hold_shadow_parent(&base.join(r"gp\parent")).is_err(), "路徑經過 junction");
        assert!(hold_shadow_parent(&base.join(r"gp2\parent")).is_ok());
    }

    #[test]
    fn install_lock_is_exclusive_across_threads() {
        let key = format!(r"c:\test\{}", uuid::Uuid::new_v4());
        let held = InstallLock::acquire(&key).expect("first acquire");
        let other = key.clone();
        let busy = std::thread::spawn(move || InstallLock::acquire(&other).is_none())
            .join()
            .unwrap();
        assert!(busy);
        drop(held);
        let free = std::thread::spawn(move || InstallLock::acquire(&key).is_some())
            .join()
            .unwrap();
        assert!(free);
    }

    // ── 竄改與預寫（直接呼叫 helper 本體，一般權限、不經 UAC）────────────────────
    //
    // HKLM 解析（`game::detect_install`）在測試中無法替換。job 與進度檔都有效時，helper 先取安裝的 named
    // mutex 才解析 HKLM；測試執行緒預先持有該 mutex，有效的 job 因此停在 11（互斥被佔），在那之前就被拒的
    // 回 10，藉此區分「因竄改被拒」與「因 HKLM 不符被拒」。HKLM 之後的再核對與套用見 `patch::apply` 的測試；
    // HKLM 交叉核對與提權後實際寫入只能手測。

    /// parent 端的狀態：假國際服安裝、已下載的實際 patch、空進度檔與 `write_job` 寫出的 job。
    struct Parent {
        dir: tempfile::TempDir,
        install: PathBuf,
        job: Job,
        job_path: PathBuf,
        sha256: String,
        progress: PathBuf,
    }

    impl Parent {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let install = dir.path().join("FINAL FANTASY XIV - A Realm Reborn");
            std::fs::create_dir_all(install.join(r"game\sqpack\ex1")).unwrap();
            std::fs::write(install.join(r"game\sqpack\ex1\ex1.ver"), OLD_VERSION).unwrap();
            let patch = dir.path().join(r"patches\global\ex1\D2026.08.07.0000.0000.patch");
            std::fs::create_dir_all(patch.parent().unwrap()).unwrap();
            std::fs::write(&patch, TW_EX1_PATCH).unwrap();
            let job_dir = dir.path().join(r"patches\global\job-1");
            std::fs::create_dir_all(&job_dir).unwrap();
            let progress = job::progress_path(&job_dir);
            std::fs::write(&progress, b"").unwrap();
            let job = Job {
                expected_install: install.clone(),
                patches: vec![JobPatch {
                    path: patch,
                    repo: Repo::Expansion(1),
                    version_id: EX1_VERSION.into(),
                    length: TW_EX1_PATCH.len() as u64,
                    hash_block_size: 50_000_000,
                    hashes: vec!["625499a52faf29e527543ee969e17ff9a81edcb6".into()],
                }],
            };
            let (job_path, sha256) = write_job(&job_dir, &job).unwrap();
            Self {
                dir,
                install,
                job,
                job_path,
                sha256,
                progress,
            }
        }

        /// 持有安裝 mutex 時在另一執行緒跑 helper（同一執行緒可重入 mutex，不會被擋）。
        fn run_locked(&self, job_path: &Path, sha256: &str) -> i32 {
            let _held = InstallLock::acquire(&normalize_path(&self.install).unwrap()).unwrap();
            let (job_path, sha256) = (job_path.to_path_buf(), sha256.to_string());
            std::thread::spawn(move || run_helper(&job_path, &sha256))
                .join()
                .unwrap()
        }

        /// 在 `name` 子目錄寫一份自造的 job，回傳路徑與它自己的 SHA-256。
        fn forge(&self, name: &str, job: &Job) -> (PathBuf, String) {
            let dir = self.dir.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let bytes = serde_json::to_vec(job).unwrap();
            let path = dir.join("job.json");
            std::fs::write(&path, &bytes).unwrap();
            (path, sha256_hex(&bytes).unwrap())
        }
    }

    /// 驗收「竄改 job 路徑被拒，且遊戲目錄不變」：落地後改 job 內的安裝或 patch 路徑、命令列的 job 路徑換成
    /// 另一份 job、自造 job（自算 SHA-256）但路徑含 `..`，都在取得安裝 mutex 前以 10 結束。
    #[test]
    fn tampered_job_is_rejected_before_install_lock_and_game_untouched() {
        let parent = Parent::new();
        let before = snapshot(&parent.install);
        assert_eq!(
            parent.run_locked(&parent.job_path, &parent.sha256),
            EXIT_BUSY,
            "未竄改的 job 通過 job 與進度檔檢查"
        );

        let original = std::fs::read(&parent.job_path).unwrap();
        let elsewhere = parent.dir.path().join("elsewhere");
        for (name, pointer, replacement) in [
            ("安裝路徑", "/expectedInstall", serde_json::json!(elsewhere)),
            ("patch 路徑", "/patches/0/path", serde_json::json!(elsewhere.join("x.patch"))),
        ] {
            let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
            *value.pointer_mut(pointer).unwrap() = replacement;
            std::fs::write(&parent.job_path, serde_json::to_vec(&value).unwrap()).unwrap();
            assert_eq!(
                parent.run_locked(&parent.job_path, &parent.sha256),
                EXIT_ABORTED_BEFORE_WRITE,
                "{name}"
            );
        }
        std::fs::write(&parent.job_path, &original).unwrap();

        let mut swapped = parent.job.clone();
        swapped.expected_install = elsewhere.clone();
        let (swapped_path, _) = parent.forge("swapped", &swapped);
        assert_eq!(
            parent.run_locked(&swapped_path, &parent.sha256),
            EXIT_ABORTED_BEFORE_WRITE,
            "命令列的 job 路徑換成另一份 job"
        );

        let mut dotted = parent.job.clone();
        dotted.patches[0].path = parent.dir.path().join(r"patches\..\x.patch");
        let (dotted_path, dotted_sha256) = parent.forge("dotted", &dotted);
        assert_eq!(
            parent.run_locked(&dotted_path, &dotted_sha256),
            EXIT_ABORTED_BEFORE_WRITE,
            "自造 job 的路徑含 .."
        );

        assert_eq!(snapshot(&parent.install), before);
        assert_eq!(std::fs::read(&parent.progress).unwrap(), b"");
    }

    /// 驗收「正常情況仍可寫」：parent 建立的 job 目錄通過核對，進度寫進 job 目錄的 progress.json；路徑中間
    /// 經過 junction（例如使用者資料夾被重新導向）不影響，因為比對的是 job 目錄本身的最終路徑。
    #[test]
    fn parent_job_dir_passes_and_progress_is_written_there() {
        let parent = Parent::new();
        let via = parent.dir.path().join("via");
        junction(&via, &parent.dir.path().join("patches"));
        let line = ApplyProgress {
            patch_index: 0,
            patch_count: 1,
            done: 1,
            total: 2,
        };
        for job_path in [parent.job_path.clone(), via.join(r"global\job-1\job.json")] {
            let (job, mut progress) = open_job(&job_path, &parent.sha256).unwrap();
            assert_eq!(job, parent.job);
            progress.write(line);
            drop(progress);
            let text = std::fs::read_to_string(&parent.progress).unwrap();
            assert_eq!(serde_json::from_str::<ApplyProgress>(&text).unwrap(), line);
            std::fs::write(&parent.progress, b"").unwrap();
        }
    }

    /// 驗收「進度檔指向 job 目錄外被拒，且目標空檔仍為 0 bytes」：自造 job 帶 progressPath 指向別處（未知
    /// 欄位）、job 目錄的 progress.json 換成別處空檔的硬連結，都在取得安裝 mutex 前以 10 結束。
    #[test]
    fn progress_outside_job_dir_is_rejected_and_target_untouched() {
        let parent = Parent::new();
        let before = snapshot(&parent.install);
        // 代表 job 目錄外、提權身分才可寫的既有空檔（例如 boot\ 裡的檔案）。
        let victim = parent.dir.path().join(r"elsewhere\victim.dat");
        std::fs::create_dir_all(victim.parent().unwrap()).unwrap();
        std::fs::write(&victim, b"").unwrap();

        let original = std::fs::read(&parent.job_path).unwrap();
        let mut forged = serde_json::to_value(&parent.job).unwrap();
        forged["progressPath"] = serde_json::json!(victim);
        let bytes = serde_json::to_vec(&forged).unwrap();
        std::fs::write(&parent.job_path, &bytes).unwrap();
        assert_eq!(
            parent.run_locked(&parent.job_path, &sha256_hex(&bytes).unwrap()),
            EXIT_ABORTED_BEFORE_WRITE,
            "job 指定進度檔路徑"
        );
        std::fs::write(&parent.job_path, &original).unwrap();

        std::fs::remove_file(&parent.progress).unwrap();
        std::fs::hard_link(&victim, &parent.progress).unwrap();
        assert_eq!(
            parent.run_locked(&parent.job_path, &parent.sha256),
            EXIT_ABORTED_BEFORE_WRITE,
            "進度檔是別處空檔的硬連結"
        );
        assert_eq!(std::fs::read(&victim).unwrap(), b"");
        assert_eq!(snapshot(&parent.install), before);

        std::fs::remove_file(&parent.progress).unwrap();
        std::fs::write(&parent.progress, b"").unwrap();
        assert_eq!(parent.run_locked(&parent.job_path, &parent.sha256), EXIT_BUSY);
    }

    /// 驗收「job 目錄在 UAC 期間被換成 junction 被拒，且目標空檔仍為 0 bytes」：目標目錄放同一份 job.json
    /// （SHA-256 相符）與空的 progress.json，命令列不變，helper 仍在取得安裝 mutex 前以 10 結束。
    #[test]
    fn junctioned_job_dir_is_rejected_and_target_untouched() {
        let parent = Parent::new();
        let before = snapshot(&parent.install);
        let job_dir = parent.job_path.parent().unwrap();
        let target = parent.dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::copy(&parent.job_path, target.join("job.json")).unwrap();
        let target_progress = job::progress_path(&target);
        std::fs::write(&target_progress, b"").unwrap();

        std::fs::rename(job_dir, parent.dir.path().join(r"patches\global\job-1-moved")).unwrap();
        junction(job_dir, &target);
        assert_eq!(
            parent.run_locked(&parent.job_path, &parent.sha256),
            EXIT_ABORTED_BEFORE_WRITE
        );
        assert_eq!(std::fs::read(&target_progress).unwrap(), b"");
        assert_eq!(snapshot(&parent.install), before);
    }

    /// 驗收「預寫成功結果被拒，且遊戲目錄不變」（helper 端）：進度檔預先寫成「已完成」時，helper 在取得安裝
    /// mutex 前以 10 結束且不覆寫它；換回 parent 建立的空檔，同一份 job 就通過檢查。parent 端只看 exit code，
    /// 見 `patch::update` 的 `helper_exit_classification_matches_dirty_policy`。
    #[test]
    fn prewritten_success_progress_is_rejected_before_install_lock() {
        let parent = Parent::new();
        let before = snapshot(&parent.install);
        let done = br#"{"patchIndex":0,"patchCount":1,"done":52848,"total":52848}"#;
        std::fs::write(&parent.progress, done).unwrap();

        assert_eq!(
            parent.run_locked(&parent.job_path, &parent.sha256),
            EXIT_ABORTED_BEFORE_WRITE
        );
        assert_eq!(std::fs::read(&parent.progress).unwrap(), done, "不覆寫");
        assert_eq!(snapshot(&parent.install), before);

        std::fs::write(&parent.progress, b"").unwrap();
        assert_eq!(parent.run_locked(&parent.job_path, &parent.sha256), EXIT_BUSY);
    }

    #[test]
    fn parse_args_accepts_shadow_form_only_with_plain_absolute_paths() {
        let (install, pack) = (r"C:\Program Files (x86)\SquareEnix\FFXIV", r"C:\Games\xlm-pack");
        assert_eq!(
            parse_args(&args(&[SHADOW_FLAG, install, pack])),
            Invocation::Shadow {
                install: PathBuf::from(install),
                pack: PathBuf::from(pack),
            }
        );
        for bad in [
            vec![SHADOW_FLAG, install],
            vec![SHADOW_FLAG, install, pack, "extra"],
            vec![SHADOW_FLAG, install, "pack"],
            vec![SHADOW_FLAG, r"C:\FFXIV\..\Windows", pack],
            vec![SHADOW_FLAG, install, r"C:\pack\NUL"],
            vec![install, SHADOW_FLAG, pack],
            // 兩個旗標同時出現。
            vec![SHADOW_FLAG, FLAG, pack],
            vec![FLAG, SHADOW_FLAG, SHA],
        ] {
            assert_eq!(parse_args(&args(&bad)), Invocation::Invalid, "{bad:?}");
        }
    }

    /// 影子 helper 先取安裝 mutex 才解析 HKLM（同 patch helper）：持有 mutex 時有效參數停在 11，安裝路徑不存在
    /// 則在那之前以 10 結束。HKLM 之後的步驟只能手測。
    #[test]
    fn shadow_helper_takes_install_lock_before_hklm() {
        let parent = Parent::new();
        let pack = parent.dir.path().join("pack");
        let _held = InstallLock::acquire(&normalize_path(&parent.install).unwrap()).unwrap();
        let run = |install: PathBuf| {
            let pack = pack.clone();
            std::thread::spawn(move || run_shadow_helper(&install, &pack))
                .join()
                .unwrap()
        };
        assert_eq!(run(parent.install.clone()), EXIT_BUSY);
        assert_eq!(run(parent.dir.path().join("missing")), EXIT_ABORTED_BEFORE_WRITE);
    }
}
