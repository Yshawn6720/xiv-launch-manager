//! 啟動遊戲：經 Dalamud.Injector.exe 注入（protocols §1.5、§2.5）或不帶衛月直接啟動。
//! 注入有兩種方式（[`InjectMode`]）：`launch --mode=inject` 由 Injector 起遊戲；attach 由本程式
//! 起遊戲、等視窗出現後再 `inject -v {pid}`（與 TC 啟動器相同）。
//!
//! 不變量：
//! - 只操作 Injector 回報（或本程式直接建立）的遊戲 pid，不按程序名查找（protocols §3）。
//! - `DALAMUD_RUNTIME`、`DOTNET_ROOT`、`__COMPAT_LAYER` 只設在 Injector 子程序；attach 時
//!   `DALAMUD_RUNTIME`、`DOTNET_ROOT` 另設在本程式建立的遊戲子程序；不改使用者或系統環境變數。
//! - Injector 的 stdout／stderr 與遊戲參數不進錯誤（參數含 SID）。

use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, BufReader, Read};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAce, GetLengthSid, GetTokenInformation,
    InitializeAcl, InitializeSecurityDescriptor, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    SECURITY_DESCRIPTOR, SetSecurityDescriptorDacl, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateProcessW, GetCurrentProcess, OpenProcessToken,
    PROCESS_ALL_ACCESS, PROCESS_INFORMATION, PROCESS_VM_WRITE, ResumeThread, STARTUPINFOW,
    TerminateProcess,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, IsWindowVisible,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

use super::install::{self, INJECTOR_EXE};
use super::profile::{self, GameProcess, ProfilePaths};
use crate::error::{AppError, AppResult};

/// 與 XL 相同：Injector 啟動遊戲並注入後就結束，等太久視為失敗。
const INJECTOR_TIMEOUT: Duration = Duration::from_secs(60);
/// Injector 結束後等待第一行輸出送達的寬限。
const OUTPUT_GRACE: Duration = Duration::from_secs(2);
/// 遊戲開出後觀察是否立即結束（可能超過客戶端實例上限，spec B6）的時間窗。觀察不擋啟動：三條啟動路徑開出
/// 遊戲就回傳，由 `launch` 在背景以 [`watch_early_exit`] 觀察，提早結束時撤銷執行狀態並通知前端。
const EARLY_EXIT_WINDOW: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// 第一行輸出的上限；正常只有一行短 JSON。
const MAX_FIRST_LINE: u64 = 64 * 1024;
/// attach：等遊戲出現可見頂層視窗的上限。
const WINDOW_TIMEOUT: Duration = Duration::from_secs(60);
/// attach：視窗出現後至少再等這麼久才注入（同 TC 啟動器）。
const MIN_ATTACH_DELAY_MS: u32 = 3000;
/// attach：`delay_ms` 為 0 時交給 Injector 的延遲初始化毫秒數（同 TC 啟動器）。
const DEFAULT_ATTACH_INIT_DELAY_MS: u32 = 10_000;

/// 注入方式；由 `dalamud::prepare` 依來源決定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectMode {
    /// `launch --mode=inject`：Injector 啟動遊戲並注入。
    Launch,
    /// 本程式啟動遊戲，等視窗出現後 `inject -v {pid}`。
    Attach,
}

/// 本次要載入哪些 plugin（protocols §2.6）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PluginLoad {
    #[default]
    All,
    /// `--no-3rd-plugin`
    NoThirdParty,
    /// `--no-plugin`
    None,
}

pub struct InjectRequest {
    pub injector_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub assets_dir: PathBuf,
    pub profile: ProfilePaths,
    pub game_exe: PathBuf,
    /// 已組好的遊戲參數（國際服為加密字串，台服為明文）；含 SID，不得外流。
    pub game_args: String,
    pub language: u8,
    pub delay_ms: u32,
    pub mode: InjectMode,
    pub plugins: PluginLoad,
}

/// 依 [`InjectMode`] 注入並啟動遊戲。成功回傳遊戲程序身分；不等提早結束的觀察（[`watch_early_exit`]）。
pub async fn launch_with_dalamud(req: &InjectRequest) -> AppResult<GameProcess> {
    match req.mode {
        InjectMode::Launch => launch_via_injector(req).await,
        InjectMode::Attach => launch_then_attach(req).await,
    }
}

/// 由 Injector 啟動遊戲並注入。成功回傳 Injector 回報的遊戲程序身分。
async fn launch_via_injector(req: &InjectRequest) -> AppResult<GameProcess> {
    let exe = req.injector_dir.join(INJECTOR_EXE);
    let args = launch_args(req);
    let runtime_dir = req.runtime_dir.clone();
    let work_dir = req.injector_dir.clone();
    let outcome = install::run_blocking("執行 Injector", move || {
        run_injector(&exe, &args, &runtime_dir, &work_dir)
    })
    .await?;

    let pid = outcome.first_line.as_deref().and_then(parse_injector_output);
    let pid = match (outcome.exit_code, pid) {
        (Some(0), Some(pid)) => pid,
        (exit_code, pid) => {
            if let Some(pid) = pid {
                // 結束失敗時遊戲可能殘留，但主因是 Injector 失敗，回報該錯誤。
                // quality-allow: 失敗路徑的善後，結束不了也不能蓋掉 Injector 失敗的主因
                let _ = profile::terminate(pid);
            }
            return Err(AppError::InjectorFailed { exit_code });
        }
    };
    profile::process_identity(pid)?.ok_or(AppError::GameExitedEarly)
}

/// 自行啟動遊戲，等可見頂層視窗出現後再 `inject -v {pid}`。任何失敗都只結束這個 pid。
async fn launch_then_attach(req: &InjectRequest) -> AppResult<GameProcess> {
    let process = spawn_game(&req.game_exe, &req.game_args, &req.runtime_dir)?;
    match attach(req, &process).await {
        Ok(()) => Ok(process),
        Err(e) => {
            if profile::is_alive(&process) {
                // quality-allow: 失敗路徑的善後，結束不了也不能蓋掉注入失敗的主因
                let _ = profile::terminate(process.pid);
            }
            Err(e)
        }
    }
}

async fn attach(req: &InjectRequest, process: &GameProcess) -> AppResult<()> {
    wait_for_window(process).await?;
    let delay = req.delay_ms.max(MIN_ATTACH_DELAY_MS);
    tokio::time::sleep(Duration::from_millis(u64::from(delay))).await;
    if !profile::is_alive(process) {
        return Err(AppError::GameExitedEarly);
    }

    let exe = req.injector_dir.join(INJECTOR_EXE);
    let args = attach_args(req, process.pid);
    let runtime_dir = req.runtime_dir.clone();
    let work_dir = req.injector_dir.clone();
    let outcome = install::run_blocking("執行 Injector", move || {
        run_injector(&exe, &args, &runtime_dir, &work_dir)
    })
    .await?;
    // pid 是本程式建立的，不解析 stdout。
    match outcome.exit_code {
        Some(0) => Ok(()),
        exit_code => Err(AppError::InjectorFailed { exit_code }),
    }
}

/// 輪詢直到遊戲有可見頂層視窗。期間遊戲結束回 `GameExitedEarly`；逾時回 `InjectorFailed`。
async fn wait_for_window(process: &GameProcess) -> AppResult<()> {
    let deadline = tokio::time::Instant::now() + WINDOW_TIMEOUT;
    loop {
        if !profile::is_alive(process) {
            return Err(AppError::GameExitedEarly);
        }
        if has_visible_window(process.pid) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(AppError::InjectorFailed { exit_code: None });
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

struct WindowSearch {
    pid: u32,
    found: bool,
}

/// 該 pid 是否擁有可見的頂層視窗。
fn has_visible_window(pid: u32) -> bool {
    let mut search = WindowSearch { pid, found: false };
    // SAFETY: lparam 指向本函式存活中的 search；EnumWindows 同步呼叫回呼，返回後不再使用該指標。
    // 回呼回 FALSE 提前結束時 EnumWindows 會回錯誤，結果改看 search.found。
    // quality-allow: Win32 FFI，EnumWindows 沒有安全包裝；提前結束的錯誤是預期情況
    let _ = unsafe { EnumWindows(Some(match_visible_window), LPARAM(&raw mut search as isize)) };
    search.found
}

unsafe extern "system" fn match_visible_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam 由 has_visible_window 傳入，指向列舉期間存活且無其他借用的 WindowSearch。
    // quality-allow: Win32 FFI 回呼，只能經 LPARAM 傳遞狀態
    let search = unsafe { &mut *(lparam.0 as *mut WindowSearch) };
    let mut owner = 0u32;
    // SAFETY: hwnd 由 EnumWindows 提供；owner 指向本函式存活中的區域變數。
    // quality-allow: Win32 FFI，GetWindowThreadProcessId／IsWindowVisible 沒有安全包裝
    let visible = unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut owner)) != 0
            && owner == search.pid
            && IsWindowVisible(hwnd).as_bool()
    };
    if visible {
        search.found = true;
        return BOOL::from(false);
    }
    BOOL::from(true)
}

/// 不帶衛月直接啟動遊戲（工作目錄＝執行檔所在目錄）。開出遊戲就回傳，不等提早結束的觀察（[`watch_early_exit`]）。
///
/// 遊戲程序的 DACL 不給目前使用者 `PROCESS_VM_WRITE`：國際服客戶端發現 DACL 允許 VM_WRITE 時會以
/// 受限 DACL 重新啟動自己，原 pid 隨即結束（被誤判成提早結束，也追丟遊戲）。做法同 XIVLauncher
/// `NativeAclFix.LaunchGame`。
pub async fn launch_plain(game_exe: &Path, game_args: &str) -> AppResult<GameProcess> {
    spawn_without_vm_write(game_exe, split_game_args(game_args), game_work_dir(game_exe)?)
}

fn game_work_dir(game_exe: &Path) -> AppResult<&Path> {
    game_exe.parent().ok_or_else(|| AppError::GameFileMissing {
        path: game_exe.display().to_string(),
    })
}

/// attach 用：啟動遊戲（工作目錄＝執行檔所在目錄、預設 DACL，Injector 之後要以同使用者寫入遊戲
/// 記憶體）並取得程序身分。`runtime_dir` 設在遊戲子程序：遊戲內的 Dalamud.Boot 讀它找 .NET runtime。
fn spawn_game(game_exe: &Path, game_args: &str, runtime_dir: &Path) -> AppResult<GameProcess> {
    let child = Command::new(game_exe)
        .args(split_game_args(game_args))
        .current_dir(game_work_dir(game_exe)?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("DALAMUD_RUNTIME", runtime_dir)
        .env("DOTNET_ROOT", runtime_dir)
        .spawn()
        .map_err(|e| AppError::io("啟動遊戲", e))?;
    // 持有 child handle 直到取得建立時間：程序即使秒退，pid 也不會在這之前被重用。
    let identity = profile::process_identity(child.id());
    drop(child);
    identity?.ok_or(AppError::GameExitedEarly)
}

/// 授予目前使用者的程序權限：全部權限去掉 `PROCESS_VM_WRITE`（0x1FFFDF，同 XL 的
/// `STANDARD_RIGHTS_ALL | SPECIFIC_RIGHTS_ALL & ~PROCESS_VM_WRITE`）。
const RESTRICTED_PROCESS_ACCESS: u32 = PROCESS_ALL_ACCESS.0 & !PROCESS_VM_WRITE.0;

/// 以只有一筆 ACE（目前使用者：[`RESTRICTED_PROCESS_ACCESS`]）的 DACL 建立 suspended 程序，
/// 持有 handle 取得程序身分後才恢復執行。不繼承任何 handle（含 stdio）、環境變數沿用本程式。
/// 建立後任何一步失敗都結束這個程序。
fn spawn_without_vm_write<'a>(
    exe: &Path,
    args: impl IntoIterator<Item = &'a str>,
    work_dir: &Path,
) -> AppResult<GameProcess> {
    let launch_error = |e: windows::core::Error| AppError::io("啟動遊戲", e.into());
    let application = wide_nul(exe.as_os_str()).map_err(|e| AppError::io("啟動遊戲", e))?;
    let current_dir = wide_nul(work_dir.as_os_str()).map_err(|e| AppError::io("啟動遊戲", e))?;
    let mut command_line = command_line(exe, args).map_err(|e| AppError::io("啟動遊戲", e))?;
    let dacl = RestrictedDacl::for_current_user().map_err(launch_error)?;

    let mut descriptor = SECURITY_DESCRIPTOR::default();
    let descriptor_ptr = PSECURITY_DESCRIPTOR((&raw mut descriptor).cast());
    // SAFETY: descriptor_ptr 指向本函式存活中的 SECURITY_DESCRIPTOR；dacl 的緩衝區活得比 descriptor
    // 的所有使用（到 CreateProcessW 返回）都久。
    // quality-allow: Win32 FFI，security descriptor API 沒有安全包裝
    unsafe {
        InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION).and_then(|()| {
            SetSecurityDescriptorDacl(descriptor_ptr, true, Some(dacl.as_ptr()), false)
        })
    }
    .map_err(launch_error)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor_ptr.0,
        bInheritHandle: false.into(),
    };
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: application／current_dir 是以 NUL 結尾的 UTF-16；command_line 是本函式持有、可寫且以
    // NUL 結尾的緩衝區（CreateProcessW 可能就地修改）；attributes／startup／info 都是存活中的區域變數。
    // quality-allow: Win32 FFI，CreateProcessW 沒有安全包裝
    unsafe {
        CreateProcessW(
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            Some(&raw const attributes),
            None,
            false,
            CREATE_SUSPENDED,
            None,
            PCWSTR(current_dir.as_ptr()),
            &raw const startup,
            &raw mut info,
        )
    }
    .map_err(launch_error)?;
    // SAFETY: 兩個 handle 都是 CreateProcessW 剛回傳、由本函式獨占的有效 handle，之後只由 OwnedHandle 關閉。
    // quality-allow: Win32 FFI，接手 CreateProcessW 回傳的原始 handle
    let (process, thread) = unsafe {
        (
            OwnedHandle::from_raw_handle(info.hProcess.0),
            OwnedHandle::from_raw_handle(info.hThread.0),
        )
    };

    // 持有 process handle 直到取得建立時間：pid 不會在這之前被重用。
    let started = profile::process_identity(info.dwProcessId).and_then(|identity| {
        let identity = identity.ok_or(AppError::GameExitedEarly)?;
        // SAFETY: thread 是本函式持有、含 THREAD_SUSPEND_RESUME 的主執行緒 handle。
        // quality-allow: Win32 FFI，ResumeThread 沒有安全包裝
        if unsafe { ResumeThread(HANDLE(thread.as_raw_handle())) } == u32::MAX {
            return Err(launch_error(windows::core::Error::from_thread()));
        }
        Ok(identity)
    });
    if started.is_err() {
        // SAFETY: process 是本函式建立並持有的程序 handle（含 PROCESS_TERMINATE）。
        // quality-allow: Win32 FFI；失敗路徑的善後，結束不了也不能蓋掉主因
        let _ = unsafe { TerminateProcess(HANDLE(process.as_raw_handle()), 1) };
    }
    started
}

/// 只有一筆 ACE 的 DACL：目前使用者（程序 token 的 TokenUser）取得 [`RESTRICTED_PROCESS_ACCESS`]。
/// 以 `u32` 陣列持有，滿足 ACL 的 DWORD 對齊；Drop 時一併釋放。
struct RestrictedDacl(Vec<u32>);

impl RestrictedDacl {
    fn for_current_user() -> windows::core::Result<Self> {
        let token_user = current_token_user()?;
        // SAFETY: token_user 以 usize 對齊、內容由 GetTokenInformation(TokenUser) 寫入，開頭即為
        // TOKEN_USER，其 SID 指標指向同一緩衝區內；緩衝區在本函式結束前都存活。
        // quality-allow: Win32 FFI，TOKEN_USER 只能由原始緩衝區轉型取得
        let sid = unsafe { (*token_user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        // SAFETY: sid 是有效的 SID（見上）。
        // quality-allow: Win32 FFI，GetLengthSid 沒有安全包裝
        let sid_len = unsafe { GetLengthSid(sid) } as usize;
        // ACCESS_ALLOWED_ACE 的 SidStart 欄位是 SID 的第一個 DWORD。
        let len = size_of::<ACL>() + size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>() + sid_len;
        let mut buf = vec![0u32; len.div_ceil(size_of::<u32>())];
        let acl = buf.as_mut_ptr().cast::<ACL>();
        // SAFETY: acl 指向本函式持有、DWORD 對齊且長度為 buf 位元組數的緩衝區；sid 在呼叫期間有效，
        // AddAccessAllowedAce 會把 SID 複製進 ACL。
        // quality-allow: Win32 FFI，ACL API 沒有安全包裝
        unsafe {
            InitializeAcl(acl, (buf.len() * size_of::<u32>()) as u32, ACL_REVISION)?;
            AddAccessAllowedAce(acl, ACL_REVISION, RESTRICTED_PROCESS_ACCESS, sid)?;
        }
        Ok(Self(buf))
    }

    fn as_ptr(&self) -> *const ACL {
        self.0.as_ptr().cast()
    }
}

/// 目前程序 token 的 TokenUser 資訊；以 usize 陣列持有以滿足 TOKEN_USER 的指標對齊。
fn current_token_user() -> windows::core::Result<Vec<usize>> {
    let mut raw = HANDLE::default();
    // SAFETY: GetCurrentProcess 回傳 pseudo-handle；raw 是存活中的區域變數。
    // quality-allow: Win32 FFI，OpenProcessToken 沒有安全包裝
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut raw) }?;
    // SAFETY: raw 是 OpenProcessToken 剛回傳、由本函式獨占的 token handle。
    // quality-allow: Win32 FFI，接手原始 handle 以便自動關閉
    let token = unsafe { OwnedHandle::from_raw_handle(raw.0) };
    let token = HANDLE(token.as_raw_handle());
    let mut len = 0u32;
    // SAFETY: 不給緩衝區只查所需長度；len 是存活中的區域變數。
    // quality-allow: Win32 FFI；查長度時預期回 ERROR_INSUFFICIENT_BUFFER，真正的錯誤由下一次呼叫回報
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &raw mut len) };
    let mut buf = vec![0usize; (len as usize).div_ceil(size_of::<usize>())];
    // SAFETY: buf 至少 len 位元組、由本函式持有；len 是存活中的區域變數。
    // quality-allow: Win32 FFI，GetTokenInformation 沒有安全包裝
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            len,
            &raw mut len,
        )
    }?;
    Ok(buf)
}

/// 以 NUL 結尾的 UTF-16；內含 NUL 時拒絕（Win32 會在該處截斷）。
fn wide_nul(s: &OsStr) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = s.encode_wide().collect();
    if wide.contains(&0) {
        return Err(nul_error());
    }
    wide.push(0);
    Ok(wide)
}

fn nul_error() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "路徑或參數含 NUL 字元")
}

/// 依 std `Command` 的規則組 CreateProcessW 命令列（以 NUL 結尾）：程式名一律加引號；參數為空或含
/// 空白／tab 才加引號，引號前的反斜線加倍並跳脫引號。遊戲依 Windows 命令列規則解析回相同的 argv。
fn command_line<'a>(exe: &Path, args: impl IntoIterator<Item = &'a str>) -> io::Result<Vec<u16>> {
    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    let mut cmd = vec![QUOTE];
    cmd.extend(exe.as_os_str().encode_wide());
    cmd.push(QUOTE);
    for arg in args {
        cmd.push(u16::from(b' '));
        let quote = arg.is_empty() || arg.contains([' ', '\t']);
        if quote {
            cmd.push(QUOTE);
        }
        let mut backslashes = 0;
        for unit in arg.encode_utf16() {
            if unit == BACKSLASH {
                backslashes += 1;
            } else {
                if unit == QUOTE {
                    cmd.extend(std::iter::repeat_n(BACKSLASH, backslashes + 1));
                }
                backslashes = 0;
            }
            cmd.push(unit);
        }
        if quote {
            cmd.extend(std::iter::repeat_n(BACKSLASH, backslashes));
            cmd.push(QUOTE);
        }
    }
    if cmd.contains(&0) {
        return Err(nul_error());
    }
    cmd.push(0);
    Ok(cmd)
}

/// 遊戲開出後觀察 [`EARLY_EXIT_WINDOW`]，期間遊戲結束回 `GameExitedEarly`。
pub async fn watch_early_exit(process: GameProcess) -> AppResult<()> {
    watch_exit_within(process, EARLY_EXIT_WINDOW).await
}

async fn watch_exit_within(process: GameProcess, window: Duration) -> AppResult<()> {
    let deadline = tokio::time::Instant::now() + window;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(POLL_INTERVAL).await;
        if !profile::is_alive(&process) {
            return Err(AppError::GameExitedEarly);
        }
    }
    Ok(())
}

/// `launch` 參數順序同 protocols §1.5。用 argv 陣列交給 Command，不組 shell 字串。
fn launch_args(req: &InjectRequest) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "launch".into(),
        "--mode=inject".into(),
        path_arg("--game=", &req.game_exe),
    ];
    push_dalamud_args(&mut args, req, req.delay_ms);
    args.push("--".into());
    args.extend(split_game_args(&req.game_args).map(OsString::from));
    args
}

/// `inject -v {pid}` 參數，同 TC 啟動器：不傳開發插件目錄、`--fix-acl`、`--se-debug-privilege`。
fn attach_args(req: &InjectRequest, pid: u32) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["inject".into(), "-v".into(), pid.to_string().into()];
    let delay_ms = if req.delay_ms == 0 {
        DEFAULT_ATTACH_INIT_DELAY_MS
    } else {
        req.delay_ms
    };
    push_dalamud_args(&mut args, req, delay_ms);
    args
}

fn push_dalamud_args(args: &mut Vec<OsString>, req: &InjectRequest, delay_ms: u32) {
    args.extend([
        path_arg("--dalamud-working-directory=", &req.injector_dir),
        path_arg("--dalamud-configuration-path=", &req.profile.config_file),
        path_arg("--logpath=", &req.profile.log_dir),
        path_arg("--dalamud-plugin-directory=", &req.profile.plugin_dir),
        path_arg("--dalamud-asset-directory=", &req.assets_dir),
        format!("--dalamud-client-language={}", req.language).into(),
        format!("--dalamud-delay-initialize={delay_ms}").into(),
    ]);
    match req.plugins {
        PluginLoad::All => {}
        PluginLoad::NoThirdParty => args.push("--no-3rd-plugin".into()),
        PluginLoad::None => args.push("--no-plugin".into()),
    }
}

fn path_arg(key: &str, path: &Path) -> OsString {
    let mut arg = OsString::from(key);
    arg.push(path);
    arg
}

/// XL 把遊戲參數原樣接在 `--` 後（WindowsDalamudRunner），由 Injector 依 Windows 命令列規則
/// 切成多個 argv 再轉給遊戲。兩服參數都不含引號（國際服是單一加密字串，台服是空白分隔的
/// key=value），依空白切開得到相同的 argv。
fn split_game_args(args: &str) -> impl Iterator<Item = &str> {
    args.split_whitespace()
}

#[derive(Deserialize)]
struct InjectorOutput {
    #[serde(alias = "Pid")]
    pid: u32,
}

/// Injector 成功時 stdout 第一行是 `{"pid":…,"handle":…}`；解析不出非零 pid 一律視為失敗。
fn parse_injector_output(line: &str) -> Option<u32> {
    let line = line.trim_start_matches('\u{feff}').trim();
    serde_json::from_str::<InjectorOutput>(line)
        .ok()
        .map(|o| o.pid)
        .filter(|&pid| pid != 0)
}

struct InjectorOutcome {
    exit_code: Option<i32>,
    first_line: Option<String>,
}

/// 在 blocking 執行緒跑 Injector。不靠 stdout EOF 判斷結束：遊戲可能繼承管線，EOF 要等遊戲關閉。
fn run_injector(
    exe: &Path,
    args: &[OsString],
    runtime_dir: &Path,
    work_dir: &Path,
) -> AppResult<InjectorOutcome> {
    let mut child = Command::new(exe)
        .args(args)
        .current_dir(work_dir)
        .env("DALAMUD_RUNTIME", runtime_dir)
        .env("DOTNET_ROOT", runtime_dir)
        .env("__COMPAT_LAYER", "RunAsInvoker HighDPIAware")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()
        .map_err(|e| AppError::io("啟動 Injector", e))?;
    let stdout = child.stdout.take().ok_or_else(|| AppError::Install {
        context: "Injector stdout 未建立".into(),
    })?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || read_first_line(stdout, tx));

    let Some(status) = wait_with_timeout(&mut child, INJECTOR_TIMEOUT)? else {
        // 逾時：結束 Injector；若它已回報 pid，交給呼叫端只結束那個遊戲程序。
        // quality-allow: Injector 可能剛好自行結束，kill 失敗不影響逾時結論
        let _ = child.kill();
        // quality-allow: 只為回收 Injector 程序資源，結果不影響逾時結論
        let _ = child.wait();
        return Ok(InjectorOutcome {
            exit_code: None,
            first_line: rx.try_recv().ok(),
        });
    };
    Ok(InjectorOutcome {
        exit_code: status.code(),
        first_line: rx.recv_timeout(OUTPUT_GRACE).ok(),
    })
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> AppResult<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| AppError::io("等待 Injector", e))?
        {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 送出第一行後繼續讀到 EOF，避免 Injector 因管線塞滿而卡住。讀取失敗或沒有輸出時什麼都不送，
/// 呼叫端會把「沒有第一行」當成 Injector 失敗。
fn read_first_line(stdout: ChildStdout, tx: mpsc::Sender<String>) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    if let Ok(n) = (&mut reader).take(MAX_FIRST_LINE).read_until(b'\n', &mut line)
        && n > 0
    {
        // quality-allow: 收端逾時離開後送不出去是預期情況，此時結果已不需要
        let _ = tx.send(String::from_utf8_lossy(&line).into_owned());
    }
    // quality-allow: 只為排空管線，內容與錯誤都不需要
    let _ = io::copy(&mut reader, &mut io::sink());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injector_output_first_line_json() {
        assert_eq!(
            parse_injector_output("{\"pid\": 1234, \"handle\": 5678}\r\n"),
            Some(1234)
        );
        assert_eq!(parse_injector_output("\u{feff}{\"Pid\":42}"), Some(42));
        assert_eq!(parse_injector_output("Unhandled exception. at Foo"), None);
        assert_eq!(parse_injector_output(""), None);
        assert_eq!(parse_injector_output("{\"handle\": 1}"), None);
        assert_eq!(parse_injector_output("{\"pid\": 0}"), None);
        assert_eq!(parse_injector_output("{\"pid\": -1}"), None);
    }

    fn sample_request(delay_ms: u32, mode: InjectMode) -> InjectRequest {
        let base = PathBuf::from(r"C:\data");
        InjectRequest {
            injector_dir: base.join(r"dalamud\yanmucorp\26-10-03-01"),
            runtime_dir: base.join(r"runtime\9.0.11"),
            assets_dir: base.join(r"assets\yanmucorp\2"),
            profile: ProfilePaths {
                config_file: base.join(r"profiles\p1\dalamudConfig.json"),
                plugin_dir: base.join(r"profiles\p1\installedPlugins"),
                dev_plugin_dir: base.join(r"profiles\p1\devPlugins"),
                log_dir: base.join(r"profiles\p1\logs"),
            },
            game_exe: PathBuf::from(r"D:\FFXIV TC\game\ffxiv_dx11.exe"),
            game_args: "DEV.LobbyHost01=neolobby01.ffxiv.com.tw  DEV.LobbyPort01=54994 DEV.TestSID=x"
                .into(),
            language: 4,
            delay_ms,
            mode,
            plugins: PluginLoad::All,
        }
    }

    fn to_strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter()
            .map(|a| a.into_string().expect("utf-8"))
            .collect()
    }

    #[test]
    fn injector_args_follow_protocol_order() {
        let req = sample_request(1500, InjectMode::Launch);
        assert_eq!(
            to_strings(launch_args(&req)),
            [
                "launch",
                "--mode=inject",
                r"--game=D:\FFXIV TC\game\ffxiv_dx11.exe",
                r"--dalamud-working-directory=C:\data\dalamud\yanmucorp\26-10-03-01",
                r"--dalamud-configuration-path=C:\data\profiles\p1\dalamudConfig.json",
                r"--logpath=C:\data\profiles\p1\logs",
                r"--dalamud-plugin-directory=C:\data\profiles\p1\installedPlugins",
                r"--dalamud-asset-directory=C:\data\assets\yanmucorp\2",
                "--dalamud-client-language=4",
                "--dalamud-delay-initialize=1500",
                "--",
                "DEV.LobbyHost01=neolobby01.ffxiv.com.tw",
                "DEV.LobbyPort01=54994",
                "DEV.TestSID=x",
            ]
        );
    }

    #[test]
    fn attach_args_follow_tc_launcher_order() {
        let expected = |delay: &str| {
            vec![
                "inject".to_string(),
                "-v".into(),
                "4321".into(),
                r"--dalamud-working-directory=C:\data\dalamud\yanmucorp\26-10-03-01".into(),
                r"--dalamud-configuration-path=C:\data\profiles\p1\dalamudConfig.json".into(),
                r"--logpath=C:\data\profiles\p1\logs".into(),
                r"--dalamud-plugin-directory=C:\data\profiles\p1\installedPlugins".into(),
                r"--dalamud-asset-directory=C:\data\assets\yanmucorp\2".into(),
                "--dalamud-client-language=4".into(),
                format!("--dalamud-delay-initialize={delay}"),
            ]
        };
        // 遊戲參數（含 SID）直接交給遊戲，不經 Injector。
        let req = sample_request(1500, InjectMode::Attach);
        assert_eq!(to_strings(attach_args(&req, 4321)), expected("1500"));
        // delay_ms 為 0 時同 TC 啟動器用 10000。
        let req = sample_request(0, InjectMode::Attach);
        assert_eq!(to_strings(attach_args(&req, 4321)), expected("10000"));
    }

    #[test]
    fn plugin_flags_go_after_dalamud_args_and_before_game_args() {
        let mut req = sample_request(1500, InjectMode::Launch);
        req.plugins = PluginLoad::NoThirdParty;
        let args = to_strings(launch_args(&req));
        let sep = args.iter().position(|a| a == "--").expect("--");
        assert_eq!(args[sep - 1], "--no-3rd-plugin");
        assert!(!args.contains(&"--no-plugin".to_string()));

        req.mode = InjectMode::Attach;
        req.plugins = PluginLoad::None;
        let args = to_strings(attach_args(&req, 4321));
        assert_eq!(args.last().map(String::as_str), Some("--no-plugin"));
        assert!(!args.contains(&"--no-3rd-plugin".to_string()));
    }

    #[test]
    fn command_line_round_trips_through_windows_parsing() {
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        use windows::Win32::UI::Shell::CommandLineToArgvW;

        let exe = Path::new(r"D:\FFXIV TC\game\ffxiv_dx11.exe");
        let args = [
            "plain",
            "two words",
            "",
            "tab\there",
            r#"q"uote"#,
            r"trail\",
            r"dir\ x\",
            r#"a\"b"#,
            "中文",
        ];
        let line = command_line(exe, args).expect("command line");
        let mut count = 0;
        // SAFETY: line 以 NUL 結尾；count 是存活中的區域變數；回傳的陣列以 LocalFree 釋放一次。
        // quality-allow: Win32 FFI，CommandLineToArgvW 沒有安全包裝
        let argv = unsafe { CommandLineToArgvW(PCWSTR(line.as_ptr()), &mut count) };
        assert!(!argv.is_null());
        // SAFETY: argv 有 count 個以 NUL 結尾的字串，在 LocalFree 前都有效。
        // quality-allow: Win32 FFI，讀取 CommandLineToArgvW 配置的原始陣列
        let parsed: Vec<String> = unsafe {
            std::slice::from_raw_parts(argv, count as usize)
                .iter()
                .map(|a| a.to_string().expect("utf-16"))
                .collect()
        };
        // SAFETY: argv 由 CommandLineToArgvW 配置，只釋放這一次。
        // quality-allow: Win32 FFI，LocalFree 沒有安全包裝
        unsafe { LocalFree(Some(HLOCAL(argv.cast()))) };
        let mut expected = vec![exe.display().to_string()];
        expected.extend(args.iter().map(|a| a.to_string()));
        assert_eq!(parsed, expected);

        assert!(command_line(exe, ["a\0b"]).is_err());
    }

    /// 不帶衛月啟動的程序拒絕目前使用者 VM_WRITE（國際服客戶端否則會自我重啟），查詢與結束仍可。
    #[test]
    fn plain_launch_denies_vm_write_to_current_user() {
        use windows::Win32::Foundation::E_ACCESSDENIED;
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        };

        // waitfor 等一個不會來的信號，期間不輸出；測完即結束。
        let system = Path::new(r"C:\Windows\System32");
        let signal = format!("xlm-acl-test-{}", std::process::id());
        let process = spawn_without_vm_write(
            &system.join("waitfor.exe"),
            ["/t", "60", signal.as_str()],
            system,
        )
        .expect("spawn");

        // SAFETY: OpenProcess 只讀值參數；成功的 handle 立即交給 OwnedHandle 獨占並關閉。
        let open = |access| {
            // quality-allow: Win32 FFI，OpenProcess 沒有安全包裝
            unsafe { OpenProcess(access, false, process.pid) }
                // quality-allow: Win32 FFI，接手 OpenProcess 剛回傳的原始 handle
                .map(|h| unsafe { OwnedHandle::from_raw_handle(h.0) })
        };
        let write = open(PROCESS_VM_WRITE).map(drop);
        let query = open(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE).map(drop);
        profile::terminate(process.pid).expect("terminate");

        assert_eq!(write.map_err(|e| e.code()), Err(E_ACCESSDENIED));
        assert!(query.is_ok());
    }

    /// 時間窗內一直在執行回 Ok；時間窗內結束回 `GameExitedEarly`。
    #[tokio::test]
    async fn early_exit_watch_reports_only_an_exit_within_the_window() {
        // 遊戲替身：waitfor 等一個不會出現的訊號（最多 120 秒）。
        let mut child = Command::new(r"C:\Windows\System32\waitfor.exe")
            .args(["/t", "120", &format!("xlm{}", uuid::Uuid::new_v4().simple())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let game = profile::process_identity(child.id())
            .expect("identity")
            .expect("stand-in alive");
        assert!(watch_exit_within(game, Duration::from_millis(600)).await.is_ok());

        let watch = tokio::spawn(watch_exit_within(game, Duration::from_secs(30)));
        child.kill().expect("kill");
        child.wait().expect("reap");
        assert!(matches!(watch.await.expect("join"), Err(AppError::GameExitedEarly)));
    }
}
