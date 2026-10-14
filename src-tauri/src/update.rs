//! 程式更新：查新版、自己分段下載、以 updater 公鑰驗 minisign 簽章，通過後依發布形式（[`Flavor`]）分流：
//! - 安裝版（NSIS 安裝的那個 exe）：交給 updater plugin 的 `install` 啟動安裝程式，本程式隨即結束，NSIS 裝完再開。
//! - 免安裝版（單一 exe）：新版下載到 exe 旁的 `<exe>.new`，執行中的 exe 改名為 `<exe>.old`、`.new` 改成原檔名，
//!   啟動新 exe、等它回報已開始執行後本程式才結束。新程序先等本程序結束才進入單一實例判定（[`finish_handoff`]），
//!   啟動後清掉 `.old`（[`remove_leftovers`]）。
//!
//! 不用 plugin 的 `download`：它只開一條連線，而 HiNet 直連 Fastly（GitHub release-assets）時單條連線
//! 常在開頭約 0.3–1 MB 後崩落到 60–240 KB/s；改用衛月下載共用的分段並行＋換線
//! （[`dalamud::download_to_file`]）。plugin 只有 `download` 會驗簽、`install` 不驗，所以照 plugin 的
//! `verify_signature` 在這裡驗。

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use minisign_verify::{PublicKey, Signature};
use serde::Serialize;
use tauri::AppHandle;
use tauri::ipc::Channel;
use tauri_plugin_updater::{Update, UpdaterExt};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, OpenEventW, OpenProcess, PROCESS_SYNCHRONIZE, SetEvent,
    WaitForMultipleObjects, WaitForSingleObject,
};
use windows::core::HSTRING;

use crate::dalamud::{self, CancelToken, Progress, Staging};
use crate::error::{AppError, AppResult};
use crate::store::AppPaths;

/// latest.json 裡免安裝版的 platform key。安裝版不指定 target，plugin 找 `windows-x86_64`（舊版程式也只認這個）。
pub const PORTABLE_TARGET: &str = "windows-x86_64-portable";

/// 新程序從這個環境變數得知要接手哪個舊程序（pid）。只由 [`hand_off`] 設給子程序，新程序一讀到就移除。
const HANDOFF_ENV: &str = "XIV_LAUNCH_MANAGER_HANDOFF_PID";
/// 舊程序等新版回報「已開始執行」的上限（毫秒）。防毒軟體第一次執行前的掃描可能要幾秒。
const SUCCESSOR_READY_TIMEOUT_MS: u32 = 30_000;
/// 新程序等舊程序結束的上限（毫秒）。舊程序收到回報就要求結束，正常一兩秒內結束；逾時仍照常往下走，舊程序若還在，
/// 單一實例會讓新程序退出，使用者重開即可。
const PREDECESSOR_EXIT_TIMEOUT_MS: u32 = 30_000;

/// `install_app_update` 經 Channel 送給前端的進度。
#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum UpdateProgress {
    /// 下載中；伺服器沒給長度時 `total` 為 `None`。
    Download { done: u64, total: Option<u64> },
    /// 已下載並通過驗簽：安裝版正在啟動安裝程式，免安裝版正在替換並啟動新版（本程式隨即結束）。
    Install,
}

/// 本程式的發布形式，決定檢查與安裝更新的方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// NSIS 安裝的那個 exe：用安裝檔更新。
    Installed,
    /// 其餘一律當免安裝版：原地替換 exe。
    Portable,
}

impl Flavor {
    /// 檢查更新時給 updater 的 target；`None`＝plugin 預設（找 `windows-x86_64`）。
    pub fn target(self) -> Option<&'static str> {
        match self {
            Flavor::Installed => None,
            Flavor::Portable => Some(PORTABLE_TARGET),
        }
    }
}

/// 判定目前的發布形式（規則與理由見 [`flavor_of`]）。
pub fn flavor(app: &AppHandle) -> Flavor {
    let (Ok(exe), Some(product)) = (std::env::current_exe(), app.config().product_name.as_deref()) else {
        return Flavor::Portable;
    };
    let (location, binary) = registered_install(product);
    flavor_of(&exe, location.as_deref(), binary.as_deref())
}

/// NSIS 安裝時寫在 HKCU 解除安裝 key 的安裝目錄（`InstallLocation`）與主程式檔名（`MainBinaryName`）。
/// tauri.conf.json 沒設 `installMode`，NSIS 用預設的 currentUser，SHCTX 就是 HKCU；key 名是 productName。
/// 讀不到的欄位為 `None`。
fn registered_install(product_name: &str) -> (Option<String>, Option<String>) {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    let path = format!(r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{product_name}");
    let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(path) else {
        return (None, None);
    };
    (key.get_value("InstallLocation").ok(), key.get_value("MainBinaryName").ok())
}

/// 目前的 exe 正是 NSIS 登記的那個 exe（安裝目錄＋主程式檔名，兩邊 canonicalize 後相同）才算安裝版，其餘都是免安裝版。
///
/// - 依據：Tauri NSIS 範本（`installer.nsi` 的 Install section）每次安裝都寫 `InstallLocation`（值帶引號）與
///   `MainBinaryName`，解除安裝時整個 key 刪掉；安裝檔更新時覆寫的也正是這個路徑。所以「目前 exe＝登記的 exe」
///   就是「用安裝檔更新會換掉自己」。
/// - 不看資料夾裡有沒有 `uninstall.exe`：免安裝 exe 放進任何有 `uninstall.exe` 的資料夾（例如別的程式的安裝目錄）
///   都會誤判。也不只比資料夾：免安裝 exe 放進安裝目錄、檔名不同時，安裝檔更新的是另一個 exe。
/// - 讀不到登錄、路徑不存在、canonicalize 失敗都當免安裝版：把免安裝版當安裝版，會用安裝檔在安裝目錄另裝一份、
///   自己永遠停在舊版；反過來只是原地換掉同一個 exe（currentUser 安裝在使用者目錄下，可寫）。
fn flavor_of(exe: &Path, install_location: Option<&str>, main_binary: Option<&str>) -> Flavor {
    let registered = install_location
        .map(|dir| dir.trim().trim_matches('"'))
        .filter(|dir| !dir.is_empty())
        .zip(main_binary.filter(|name| !name.is_empty()))
        .map(|(dir, name)| Path::new(dir).join(name));
    let installed = registered.is_some_and(|registered| {
        matches!(
            (fs::canonicalize(exe), fs::canonicalize(registered)),
            (Ok(current), Ok(registered)) if current == registered
        )
    });
    if installed { Flavor::Installed } else { Flavor::Portable }
}

/// 重新查詢最新版，下載、驗簽後依發布形式安裝（見檔頭）。呼叫端須持有 app-wide launch mutex：`staging/` 的不變量見
/// `dalamud::install`，而且兩種形式成功後本程式都會結束，不能發生在安裝或登入途中。
/// - 安裝版：安裝程式啟動成功時 updater 直接結束本程式，不會回傳。
/// - 免安裝版：新版回報已開始執行後要求本程式結束（`AppHandle::exit`）並回 Ok；失敗時 exe 已還原，回錯。
pub async fn download_and_install(
    app: &AppHandle,
    paths: &AppPaths,
    progress: Channel<UpdateProgress>,
    cancel: &CancelToken,
) -> AppResult<()> {
    match flavor(app) {
        Flavor::Installed => run_installer(app, paths, progress, cancel).await,
        Flavor::Portable => replace_portable(app, progress, cancel).await,
    }
}

/// 以該發布形式的 target 查 latest.json；沒有比目前新的版本時回 `NotFound`。
async fn check_latest(app: &AppHandle, flavor: Flavor) -> AppResult<Update> {
    let mut builder = app.updater_builder();
    if let Some(target) = flavor.target() {
        builder = builder.target(target);
    }
    builder
        .build()
        .map_err(|e| updater_error("檢查更新", e))?
        .check()
        .await
        .map_err(|e| updater_error("檢查更新", e))?
        .ok_or_else(|| AppError::NotFound {
            what: "可安裝的程式更新".into(),
        })
}

/// 下載進度轉送給前端。
fn download_reporter(progress: &Channel<UpdateProgress>) -> impl Fn(Progress) + Send + Sync + 'static {
    let channel = progress.clone();
    move |p: Progress| {
        // quality-allow: 進度只影響畫面，送不出去不影響更新本身
        let _ = channel.send(UpdateProgress::Download {
            done: p.done,
            total: p.total,
        });
    }
}

/// 安裝版：安裝檔下載到 `staging/`，驗簽後交給 updater plugin 啟動 NSIS 安裝程式。
async fn run_installer(
    app: &AppHandle,
    paths: &AppPaths,
    progress: Channel<UpdateProgress>,
    cancel: &CancelToken,
) -> AppResult<()> {
    let update = check_latest(app, Flavor::Installed).await?;
    let config = updater_config(app)?;

    let staging = Staging::create(paths, "app-update")?;
    let installer = staging.path().join("setup.exe");
    let report = download_reporter(&progress);
    let client = dalamud::http_client()?;
    dalamud::download_to_file(
        &client,
        update.download_url.as_str(),
        &installer,
        "下載更新",
        &report,
        cancel,
    )
    .await?;
    let bytes = fs::read(&installer).map_err(|e| AppError::io("讀取更新檔", e))?;
    // 安裝前就刪暫存：Windows 上 install 成功會直接結束本程式，之後沒有機會清。
    drop(staging);
    verify_signature(
        &bytes,
        &update.signature,
        &config.pubkey,
        &update.version,
        config.require_signed_version,
    )?;
    // quality-allow: 進度只影響畫面，送不出去不影響更新本身
    let _ = progress.send(UpdateProgress::Install);
    update
        .install(bytes)
        .map_err(|e| updater_error("啟動安裝程式", e))
}

/// 免安裝版：新版下載到 exe 旁的 `.new`，驗簽後替換、啟動新版，新版回報已開始執行就要求本程式結束。
/// 下載、驗簽、替換、啟動任一步失敗，exe 都維持（或還原成）目前的版本。
async fn replace_portable(
    app: &AppHandle,
    progress: Channel<UpdateProgress>,
    cancel: &CancelToken,
) -> AppResult<()> {
    let update = check_latest(app, Flavor::Portable).await?;
    let config = updater_config(app)?;
    let exe = std::env::current_exe().map_err(|e| portable_error(format!("找不到目前的執行檔：{e}")))?;
    let files = SwapPaths::beside(&exe);
    let staged = StagedExe::create(&files.new)?;
    let report = download_reporter(&progress);
    let client = dalamud::http_client()?;
    dalamud::download_to_file(
        &client,
        update.download_url.as_str(),
        staged.0,
        "下載更新",
        &report,
        cancel,
    )
    .await
    .map_err(|e| match e {
        // 寫不進 exe 旁的資料夾（磁碟滿、被防毒軟體擋下）：重試多半一樣，指向手動下載。網路錯誤與取消照舊。
        AppError::Io { context, message } => portable_error(format!("{context}：{message}")),
        other => other,
    })?;
    install_staged(
        &files,
        &update.signature,
        &config.pubkey,
        &update.version,
        config.require_signed_version,
        || {
            // quality-allow: 進度只影響畫面，送不出去不影響更新本身
            let _ = progress.send(UpdateProgress::Install);
        },
    )?;
    let successor = files.exe.clone();
    let launched = tokio::task::spawn_blocking(move || hand_off(Command::new(successor), SUCCESSOR_READY_TIMEOUT_MS))
        .await
        .unwrap_or_else(|e| Err(format!("等待新版啟動時失敗：{e}")));
    if let Err(reason) = launched {
        return Err(swap_back(&files, reason));
    }
    // 新版正在等本程序結束（finish_handoff）；exit 走 Tauri 的正常結束（單一實例 plugin 在 Exit 事件釋放 mutex）。
    app.exit(0);
    Ok(())
}

/// 替換用的三個路徑，都在 exe 所在的資料夾：同一個 volume，rename 才是原子的。
struct SwapPaths {
    exe: PathBuf,
    /// 下載、驗簽中的新版。
    new: PathBuf,
    /// 換下來的舊版：執行中的 exe 可以改名、不能刪，下次啟動時清掉。
    old: PathBuf,
}

impl SwapPaths {
    fn beside(exe: &Path) -> Self {
        let with_suffix = |suffix: &str| {
            let mut path = OsString::from(exe.as_os_str());
            path.push(suffix);
            PathBuf::from(path)
        };
        Self {
            exe: exe.to_path_buf(),
            new: with_suffix(".new"),
            old: with_suffix(".old"),
        }
    }
}

/// 下載中的 `<exe>.new`。建立時就在 exe 旁開檔：資料夾不能寫（例如放在 Program Files）時不必等下載完才失敗。
/// drop 時刪掉：下載失敗、取消、驗簽失敗、替換失敗或啟動失敗後移回的新版都不留下；換上後它已改成原檔名，不會刪到。
struct StagedExe<'a>(&'a Path);

impl<'a> StagedExe<'a> {
    fn create(path: &'a Path) -> AppResult<Self> {
        fs::File::create(path).map_err(|e| {
            portable_error(format!(
                "無法在執行檔所在的資料夾建立檔案，可能沒有寫入權限（例如放在 Program Files）：{e}"
            ))
        })?;
        Ok(Self(path))
    }
}

impl Drop for StagedExe<'_> {
    fn drop(&mut self) {
        // 刪不掉只會留下殘留檔，下次啟動的 remove_leftovers 會再清。
        if let Err(e) = remove_file_if_exists(self.0) {
            eprintln!("刪除更新暫存檔 {} 失敗：{e}", self.0.display());
        }
    }
}

/// 驗證下載好的 `.new` 再換上：驗簽沒過就刪掉 `.new`、不動 exe；通過後呼叫 `on_verified`，再 [`swap_in`]。
fn install_staged(
    files: &SwapPaths,
    signature: &str,
    pubkey: &str,
    announced_version: &str,
    require_signed_version: bool,
    on_verified: impl FnOnce(),
) -> AppResult<()> {
    let bytes = fs::read(&files.new).map_err(|e| portable_error(format!("讀取下載的新版失敗：{e}")))?;
    if let Err(e) = verify_signature(&bytes, signature, pubkey, announced_version, require_signed_version) {
        if let Err(remove) = remove_file_if_exists(&files.new) {
            eprintln!("刪除未通過驗簽的更新檔 {} 失敗：{remove}", files.new.display());
        }
        return Err(e);
    }
    drop(bytes);
    on_verified();
    swap_in(files)
}

/// exe 改名為 `.old`，再把 `.new` 改成原檔名；第二步失敗就把 `.old` 改回來，exe 維持原樣。
fn swap_in(files: &SwapPaths) -> AppResult<()> {
    fs::rename(&files.exe, &files.old).map_err(|e| portable_error(format!("無法改名目前的執行檔：{e}")))?;
    let Err(e) = fs::rename(&files.new, &files.exe) else {
        return Ok(());
    };
    let reason = format!("無法換上新版：{e}");
    Err(match fs::rename(&files.old, &files.exe) {
        Ok(()) => portable_error(reason),
        Err(restore) => stranded(files, format!("{reason}；還原時：{restore}")),
    })
}

/// 換上的新版跑不起來：新版移回 `.new`（由 [`StagedExe`] 刪掉），`.old` 改回原檔名。回傳給使用者的錯誤。
fn swap_back(files: &SwapPaths, reason: String) -> AppError {
    match fs::rename(&files.exe, &files.new).and_then(|()| fs::rename(&files.old, &files.exe)) {
        Ok(()) => portable_error(reason),
        Err(restore) => stranded(files, format!("{reason}；還原時：{restore}")),
    }
}

/// 交接事件的名稱：舊程序建立，新程序開始執行時設定。名稱帶舊程序 pid，不會與其他交接混用。
fn handoff_event_name(pid: u32) -> HSTRING {
    HSTRING::from(format!(r"Local\xiv-launch-manager-update-handoff-{pid}"))
}

/// 啟動換上的新版（不帶參數），等它回報已開始執行（[`finish_handoff`]）才回 Ok。回報前子程序就結束或逾時，
/// 代表新版跑不起來（例如被防毒軟體擋下）：先結束子程序，免得還原後它才跑起來，再回傳原因讓呼叫端還原舊版。
fn hand_off(mut command: Command, timeout_ms: u32) -> Result<(), String> {
    let pid = std::process::id();
    // SAFETY: 參數都是值或存活中的區域變數；成功的 handle 立即交給 OwnedHandle 獨占並在 drop 時關閉。
    // quality-allow: Win32 FFI，CreateEventW 沒有安全包裝
    let ready = unsafe { CreateEventW(None, true, false, &handoff_event_name(pid)) }
        // quality-allow: Win32 FFI，接手 CreateEventW 剛回傳的原始 handle
        .map(|handle| unsafe { OwnedHandle::from_raw_handle(handle.0) })
        .map_err(|e| format!("建立交接事件失敗：{e}"))?;
    let mut child = command
        .env(HANDOFF_ENV, pid.to_string())
        .spawn()
        .map_err(|e| format!("無法啟動新版：{e}"))?;
    let handles = [HANDLE(ready.as_raw_handle()), HANDLE(child.as_raw_handle())];
    // SAFETY: 兩個 handle 在等待期間都有效（ready 與 child 都在本函式結束後才 drop）。
    // quality-allow: Win32 FFI，WaitForMultipleObjects 沒有安全包裝
    let state = unsafe { WaitForMultipleObjects(&handles, false, timeout_ms) };
    if state == WAIT_OBJECT_0 {
        return Ok(());
    }
    let reason = if state.0 == WAIT_OBJECT_0.0 + 1 {
        "新版啟動後立即結束，可能被防毒軟體擋下"
    } else {
        "等不到新版回報已啟動，可能被防毒軟體擋下"
    };
    // 子程序已結束時 kill 是 no-op；wait 等它真的結束，之後才能把檔案改名回去。
    // quality-allow: 子程序可能已自行結束，kill 失敗不影響後續還原
    let _ = child.kill();
    // quality-allow: 只為等子程序結束，結束碼與回報的原因無關
    let _ = child.wait();
    Err(reason.into())
}

/// 免安裝版更新的交接（新程序端）。`main` 在 `run`（單一實例 plugin）之前呼叫；不是更新啟動的就什麼都不做。
///
/// 舊程序換上新版後啟動本程序，等到「已開始執行」的回報才結束。本程序若直接進入單一實例判定，舊程序可能還沒結束，
/// 本程序會被當成第二個實例而退出。所以先開舊程序的 handle、回報，再等它結束（最多 [`PREDECESSOR_EXIT_TIMEOUT_MS`]）。
/// 先開 handle 再回報：舊程序收到回報前不會結束，pid 不會在這之間被別的程序重用。
pub fn finish_handoff() {
    let Some(value) = std::env::var_os(HANDOFF_ENV) else {
        return;
    };
    // SAFETY: main 一開始就呼叫，還沒有其他執行緒，沒有並行的環境變數讀寫。移除是為了之後開的遊戲、ACT 等子程序
    // 不繼承。quality-allow: Rust 2024 的 remove_var 是 unsafe，單執行緒時成立
    unsafe { std::env::remove_var(HANDOFF_ENV) };
    let Some(pid) = value.to_str().and_then(|v| v.parse::<u32>().ok()) else {
        return;
    };
    // SAFETY: OpenProcess 只讀值參數；成功的 handle 立即交給 OwnedHandle 獨占並關閉。開不到（舊程序已結束）就不必等。
    // quality-allow: Win32 FFI，OpenProcess 沒有安全包裝
    let predecessor = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }
        .ok()
        // quality-allow: Win32 FFI，接手 OpenProcess 剛回傳的原始 handle
        .map(|handle| unsafe { OwnedHandle::from_raw_handle(handle.0) });
    // SAFETY: OpenEventW 只讀值參數與存活中的名稱字串；成功的 handle 立即交給 OwnedHandle。開不到代表舊程序已逾時
    // 放棄（它會結束本程序並還原），照常往下走即可。
    // quality-allow: Win32 FFI，OpenEventW 沒有安全包裝
    if let Ok(event) = unsafe { OpenEventW(EVENT_MODIFY_STATE, false, &handoff_event_name(pid)) } {
        // quality-allow: Win32 FFI，接手 OpenEventW 剛回傳的原始 handle
        let event = unsafe { OwnedHandle::from_raw_handle(event.0) };
        // SAFETY: event 是含 EVENT_MODIFY_STATE 的有效 handle。設定失敗時舊程序逾時後會自行還原，這裡無可補救。
        // quality-allow: Win32 FFI；SetEvent 失敗的後果由舊程序的逾時處理
        let _ = unsafe { SetEvent(HANDLE(event.as_raw_handle())) };
    }
    if let Some(predecessor) = predecessor {
        // SAFETY: predecessor 是含 PROCESS_SYNCHRONIZE 的有效 handle，在等待期間存活。逾時或失敗都照常啟動。
        // quality-allow: Win32 FFI；等待結果不影響後續（見 PREDECESSOR_EXIT_TIMEOUT_MS）
        let _ = unsafe { WaitForSingleObject(HANDLE(predecessor.as_raw_handle()), PREDECESSOR_EXIT_TIMEOUT_MS) };
    }
}

/// 清掉上次免安裝版更新留下的 `<exe>.old`（換下來的舊版）與 `<exe>.new`（中斷的下載）。只在單一實例判定之後呼叫
/// （`lib.rs` 的 setup）：第二個實例若在判定前清理，會刪掉第一個實例正在下載的 `.new`。舊程序還沒結束時 `.old`
/// 刪不掉，留到下次啟動。
pub fn remove_leftovers() {
    match std::env::current_exe() {
        Ok(exe) => remove_leftovers_beside(&exe),
        Err(e) => eprintln!("清除更新殘留檔：找不到目前的執行檔：{e}"),
    }
}

fn remove_leftovers_beside(exe: &Path) {
    let files = SwapPaths::beside(exe);
    for path in [&files.old, &files.new] {
        if let Err(e) = remove_file_if_exists(path) {
            eprintln!("清除更新殘留檔 {} 失敗：{e}", path.display());
        }
    }
}

/// 刪檔；檔案本來就不在不算失敗。
fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn portable_error(reason: String) -> AppError {
    AppError::PortableUpdate { reason }
}

fn stranded(files: &SwapPaths, reason: String) -> AppError {
    AppError::PortableUpdateStranded {
        reason,
        exe: files.exe.display().to_string(),
        backup: files.old.display().to_string(),
    }
}

/// tauri.conf.json 的 `plugins.updater`，用 plugin 自己的型別解析：pubkey 與 `requireSignedVersion`
/// 和 plugin 看到的相同（`lib.rs` 沒有用 `Builder::pubkey` 覆寫）。
fn updater_config(app: &AppHandle) -> AppResult<tauri_plugin_updater::Config> {
    app.config()
        .plugins
        .0
        .get("updater")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .ok_or_else(|| AppError::Updater {
            context: "讀取 updater 設定".into(),
            message: "tauri.conf.json 的 plugins.updater 缺少或格式不符".into(),
        })
}

/// updater plugin 的錯誤：網路錯誤經 [`AppError::network`] 去掉 URL；其餘是固定訊息或本機路徑。
fn updater_error(context: &str, err: tauri_plugin_updater::Error) -> AppError {
    match err {
        tauri_plugin_updater::Error::Reqwest(e) => AppError::network(context, e),
        other => AppError::Updater {
            context: context.into(),
            message: other.to_string(),
        },
    }
}

/// 驗更新檔簽章，做法同 updater plugin 的 `verify_signature`：`pubkey`（tauri.conf.json）與
/// `signature`（latest.json）都是 minisign 文字再包一層 base64。簽章通過後 trusted comment 才可信，
/// 再核對其中的簽署版本：有寫就必須等於公告版本（latest.json 本身沒簽，防止拿舊版的合法簽章配新版號）；
/// 沒寫時只有 `require_signed_version` 才拒絕。版本比對去掉開頭的 `v` 後逐字比較：合法 semver 的字串
/// 相等與 plugin 的 semver 相等等價。
pub fn verify_signature(
    data: &[u8],
    signature: &str,
    pubkey: &str,
    announced_version: &str,
    require_signed_version: bool,
) -> AppResult<()> {
    let public_key = base64_text(pubkey)
        .and_then(|text| PublicKey::decode(&text).ok())
        .ok_or_else(|| signature_error("公鑰格式不符"))?;
    let signature = base64_text(signature)
        .and_then(|text| Signature::decode(&text).ok())
        .ok_or_else(|| signature_error("簽章格式不符"))?;
    // 第三個參數同 plugin：也接受非 prehashed 的舊格式簽章。
    public_key
        .verify(data, &signature, true)
        .map_err(|_| signature_error("簽章不符"))?;
    let signed = signature
        .trusted_comment()
        .split('\t')
        .find_map(|field| field.strip_prefix("version:"));
    match signed {
        Some(v) if v.trim_start_matches('v') == announced_version.trim_start_matches('v') => Ok(()),
        Some(v) => Err(signature_error(&format!(
            "簽章綁定版本 {v}，更新資訊卻宣告 {announced_version}"
        ))),
        None if require_signed_version => Err(signature_error("簽章沒有綁定版本")),
        None => Ok(()),
    }
}

fn base64_text(s: &str) -> Option<String> {
    STANDARD
        .decode(s)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
}

fn signature_error(reason: &str) -> AppError {
    AppError::UpdateSignature {
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::os::windows::fs::OpenOptionsExt;

    use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;

    use super::*;

    // 一次性測試金鑰（`tauri signer generate`，私鑰已丟棄）以 `tauri signer sign --app-version 0.1.2`
    // 簽 FIXTURE；格式與 release.mjs 發布的 .sig／tauri.conf.json pubkey 相同（base64 包 minisign 文字）。
    const FIXTURE: &[u8] = b"xiv-launch-manager updater signature fixture\n";
    const PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDEyQzBENkMwMjVDODc2MzQKUldRMGRzZ2x3TmJBRXRqMTJzeWZXalgwbnFwc0hRSWp3LzdjL21Cc1NDc2ppR2Z0Rkh5ckNpZVQK";
    const SIGNATURE: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVRMGRzZ2x3TmJBRXBCRFBQcVJQUVMyTDVXYXA1WlVNL0wwSUxEUU9QdFcvQ0NuS05rOEhraThpZkM0Q0JJU3dzNExwNHorQmVFMzY4R0ZXcWRDcFdITkE5YWlvL2ExNEF3PQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkxODUyNDk4CWZpbGU6Zml4dHVyZS5iaW4JdmVyc2lvbjowLjEuMgpIZ0N1U0srbitkWEtrVUVXV3ByVXlPeFlxWmFHN1JpcEdnRmxpbEw4a21RYXBpbEZBcGhxbmh5eENuVjlJZTZQSk50STRjR1paVTV0WFJmRUJWbXdDUT09Cg==";
    const OLD_EXE: &[u8] = b"current version";

    #[test]
    fn genuine_signature_passes() {
        verify_signature(FIXTURE, SIGNATURE, PUBKEY, "0.1.2", true).expect("verify");
    }

    #[test]
    fn tampered_file_or_announced_version_is_rejected() {
        let rejected = |result: AppResult<()>| matches!(result, Err(AppError::UpdateSignature { .. }));
        for i in [0, FIXTURE.len() / 2, FIXTURE.len() - 1] {
            let mut tampered = FIXTURE.to_vec();
            tampered[i] ^= 1;
            assert!(rejected(verify_signature(&tampered, SIGNATURE, PUBKEY, "0.1.2", false)), "byte {i}");
        }
        // 拿 0.1.2 的合法簽章配上宣告 0.1.3 的更新資訊。
        assert!(rejected(verify_signature(FIXTURE, SIGNATURE, PUBKEY, "0.1.3", false)));
    }

    /// 暫存目錄裡的替身 exe（內容 OLD_EXE）與它的替換路徑。
    fn stand_in_exe(name: &str) -> (tempfile::TempDir, SwapPaths) {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join(name);
        fs::write(&exe, OLD_EXE).expect("write exe");
        let files = SwapPaths::beside(&exe);
        (dir, files)
    }

    #[test]
    fn swap_paths_are_siblings_with_suffix() {
        let files = SwapPaths::beside(Path::new(r"C:\Apps\XIV.Launch.Manager_0.3.0_x64-portable.exe"));
        assert_eq!(files.new, Path::new(r"C:\Apps\XIV.Launch.Manager_0.3.0_x64-portable.exe.new"));
        assert_eq!(files.old, Path::new(r"C:\Apps\XIV.Launch.Manager_0.3.0_x64-portable.exe.old"));
    }

    #[test]
    fn installed_only_when_exe_is_the_registered_one() {
        let (dir, files) = stand_in_exe("xiv-launch-manager.exe");
        let location = format!("\"{}\"", dir.path().display());
        assert_eq!(
            flavor_of(&files.exe, Some(&location), Some("xiv-launch-manager.exe")),
            Flavor::Installed
        );
        // 登錄的路徑大小寫與目前路徑不同（NTFS 不分大小寫）仍是同一個檔。
        let upper = dir.path().display().to_string().to_uppercase();
        assert_eq!(
            flavor_of(&files.exe, Some(&upper), Some("XIV-LAUNCH-MANAGER.EXE")),
            Flavor::Installed
        );
    }

    #[test]
    fn anything_else_is_portable() {
        let (dir, files) = stand_in_exe("xiv-launch-manager.exe");
        let location = dir.path().display().to_string();
        // 免安裝 exe 放進安裝目錄、檔名不同：安裝檔更新的是另一個 exe。
        let renamed = dir.path().join("XIV.Launch.Manager_0.3.0_x64-portable.exe");
        fs::write(&renamed, OLD_EXE).expect("write portable");
        assert_eq!(
            flavor_of(&renamed, Some(&location), Some("xiv-launch-manager.exe")),
            Flavor::Portable
        );
        // 放在別處，即使那個資料夾有 uninstall.exe。
        let elsewhere = tempfile::tempdir().expect("tempdir");
        fs::write(elsewhere.path().join("uninstall.exe"), b"").expect("write uninstall");
        let portable = elsewhere.path().join("xiv-launch-manager.exe");
        fs::write(&portable, OLD_EXE).expect("write portable");
        assert_eq!(
            flavor_of(&portable, Some(&location), Some("xiv-launch-manager.exe")),
            Flavor::Portable
        );
        // 沒有安裝紀錄、紀錄不完整或指向不存在的路徑。
        assert_eq!(flavor_of(&files.exe, None, None), Flavor::Portable);
        assert_eq!(flavor_of(&files.exe, Some(&location), None), Flavor::Portable);
        assert_eq!(flavor_of(&files.exe, Some("\"\""), Some("xiv-launch-manager.exe")), Flavor::Portable);
        let missing = dir.path().join("gone").display().to_string();
        assert_eq!(
            flavor_of(&files.exe, Some(&missing), Some("xiv-launch-manager.exe")),
            Flavor::Portable
        );
    }

    #[test]
    fn latest_json_platform_key_per_flavor() {
        // release 的 latest.json：舊版程式（安裝版）讀 windows-x86_64，免安裝版讀 windows-x86_64-portable。
        let manifest = serde_json::json!({
            "version": "0.3.0",
            "notes": "notes",
            "pub_date": "2026-10-15T00:00:00Z",
            "platforms": {
                "windows-x86_64": {
                    "signature": "setup-sig",
                    "url": "https://github.com/Yshawn6720/xiv-launch-manager/releases/download/v0.3.0/XIV.Launch.Manager_0.3.0_x64-setup.exe"
                },
                "windows-x86_64-portable": {
                    "signature": "portable-sig",
                    "url": "https://github.com/Yshawn6720/xiv-launch-manager/releases/download/v0.3.0/XIV.Launch.Manager_0.3.0_x64-portable.exe"
                }
            }
        });
        let release: tauri_plugin_updater::RemoteRelease = serde_json::from_value(manifest).expect("parse latest.json");
        assert_eq!(Flavor::Installed.target(), None);
        let target = Flavor::Portable.target().expect("portable target");
        assert!(release.download_url(target).expect("portable url").as_str().ends_with("_x64-portable.exe"));
        assert_eq!(release.signature(target).expect("portable sig"), "portable-sig");
        // 安裝版不指定 target 時 plugin 退回 `windows-x86_64`：多一個 key 不影響它。
        assert!(release.download_url("windows-x86_64").expect("setup url").as_str().ends_with("_x64-setup.exe"));
        assert_eq!(release.signature("windows-x86_64").expect("setup sig"), "setup-sig");
    }

    #[test]
    fn verified_update_replaces_exe_and_keeps_old() {
        let (_dir, files) = stand_in_exe("app.exe");
        fs::write(&files.new, FIXTURE).expect("write new");
        let mut verified = false;
        install_staged(&files, SIGNATURE, PUBKEY, "0.1.2", true, || verified = true).expect("install");
        assert!(verified);
        assert_eq!(fs::read(&files.exe).expect("read exe"), FIXTURE);
        assert_eq!(fs::read(&files.old).expect("read old"), OLD_EXE);
        assert!(!files.new.exists());
    }

    #[test]
    fn rejected_signature_leaves_exe_untouched() {
        let (_dir, files) = stand_in_exe("app.exe");
        let mut tampered = FIXTURE.to_vec();
        tampered[0] ^= 1;
        fs::write(&files.new, &tampered).expect("write new");
        let mut verified = false;
        let err = install_staged(&files, SIGNATURE, PUBKEY, "0.1.2", true, || verified = true).unwrap_err();
        assert!(matches!(err, AppError::UpdateSignature { .. }), "{err}");
        assert!(!verified);
        assert_eq!(fs::read(&files.exe).expect("read exe"), OLD_EXE);
        assert!(!files.new.exists(), "未通過驗簽的檔案要刪掉");
        assert!(!files.old.exists());
    }

    #[test]
    fn failed_second_rename_restores_exe() {
        let (_dir, files) = stand_in_exe("app.exe");
        fs::write(&files.new, FIXTURE).expect("write new");
        // 另一個程序（例如防毒軟體）開著 .new 且不共用刪除權：改名 .new 會失敗。
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .open(&files.new)
            .expect("hold new");
        let err = swap_in(&files).unwrap_err();
        assert!(matches!(err, AppError::PortableUpdate { .. }), "{err}");
        assert_eq!(fs::read(&files.exe).expect("read exe"), OLD_EXE);
        assert!(!files.old.exists());
        drop(held);
        assert_eq!(fs::read(&files.new).expect("read new"), FIXTURE);
    }

    #[test]
    fn swap_back_restores_old_exe() {
        let (_dir, files) = stand_in_exe("app.exe");
        fs::write(&files.new, FIXTURE).expect("write new");
        swap_in(&files).expect("swap in");
        let err = swap_back(&files, "新版啟動後立即結束".into());
        assert!(matches!(err, AppError::PortableUpdate { .. }), "{err}");
        assert_eq!(fs::read(&files.exe).expect("read exe"), OLD_EXE);
        assert!(!files.old.exists());
        // 移回 .new 的新版由 StagedExe 刪掉。
        drop(StagedExe(&files.new));
        assert!(!files.new.exists());
    }

    #[test]
    fn startup_removes_only_update_leftovers() {
        let (dir, files) = stand_in_exe("app.exe");
        fs::write(&files.old, b"old").expect("write old");
        fs::write(&files.new, b"partial").expect("write new");
        let unrelated = dir.path().join("other.exe.old");
        fs::write(&unrelated, b"keep").expect("write unrelated");
        remove_leftovers_beside(&files.exe);
        assert!(!files.old.exists());
        assert!(!files.new.exists());
        assert!(files.exe.exists());
        assert!(unrelated.exists());
        // 沒有殘留時什麼都不做。
        remove_leftovers_beside(&files.exe);
        assert!(files.exe.exists());
    }

    fn system32(exe: &str) -> PathBuf {
        let root = std::env::var_os("SystemRoot").expect("SystemRoot");
        Path::new(&root).join("System32").join(exe)
    }

    #[test]
    fn successor_exiting_before_ready_is_a_failure() {
        let mut command = Command::new(system32("cmd.exe"));
        command.args(["/c", "exit", "3"]);
        let reason = hand_off(command, 30_000).unwrap_err();
        assert!(reason.contains("立即結束"), "{reason}");
    }

    #[test]
    fn successor_never_ready_times_out_and_is_killed() {
        // 替身：等一個不會來的信號（最多 120 秒），也不會回報交接事件。
        let mut command = Command::new(system32("waitfor.exe"));
        command.args(["/t", "120", &format!("xlm{}", uuid::Uuid::new_v4().simple())]);
        let started = std::time::Instant::now();
        let reason = hand_off(command, 500).unwrap_err();
        assert!(reason.contains("等不到"), "{reason}");
        // hand_off 會結束並等子程序，不會等到 120 秒。
        assert!(started.elapsed() < std::time::Duration::from_secs(30));
    }
}
