//! 程序執行判定（主 note「遊戲執行判定」、protocols §3 例外）：以完整映像路徑比對，不按程序名。
//! 遊戲：目標安裝的 `game\ffxiv_dx11.exe` 與 `boot\` 下的執行檔，只用於更新互斥；ACT：設定的執行檔，
//! 只用於「已在執行就不再開」與畫面狀態。都不用於查找、等待或結束程序。唯一按名稱判定的是
//! [`any_named`]：ACT 一鍵設定前確認沒有任何 ACT 在跑（protocols §6.4）。
//!
//! 開不起來的程序（權限不足，例如其他使用者的程序）拿不到映像路徑：映像名稱與目標執行檔同名時
//! 保守視為執行中（寧可誤擋更新，也不在遊戲執行時寫入；ACT 寧可不開，也不多開一個）；名稱不同的
//! 程序，映像路徑不可能是目標，直接略過。

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, HANDLE,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::{HRESULT, PWSTR};

use crate::error::{AppError, AppResult};

const GAME_EXE: &str = "ffxiv_dx11.exe";
/// `QueryFullProcessImageNameW` 的緩衝區大小（UTF-16 單位；Windows 路徑長度上限）。
const IMAGE_PATH_CAP: u32 = 32_768;

/// `install`（安裝根目錄）的 `game\ffxiv_dx11.exe` 或 `boot\` 下任一 `.exe` 是否有程序正在執行。
/// 映像名稱相符但無法確認路徑的程序算使用中（見模組說明）。
pub fn install_in_use(install: &Path) -> AppResult<bool> {
    let targets = Targets::new(&normalize_path(install)?);
    let names = image_names(install)?;
    any_image_matches(&names, |image| targets.matches(image))
}

/// 完整路徑為 `exe` 的程式是否有程序正在執行；映像名稱相符但無法確認路徑的程序算執行中（見模組說明）。
/// `exe` 不存在即失敗。
pub(crate) fn exe_running(exe: &Path) -> AppResult<bool> {
    let target = normalize_path(exe)?;
    // 取解析後的檔名：經 symlink 啟動時，程序的映像名稱是實際檔案的名稱。
    let name = target.rsplit('\\').next().unwrap_or(&target).to_string();
    any_image_matches(&HashSet::from([name]), |image| image == target)
}

/// 是否有映像名稱（不分大小寫）在 `names`（小寫）中的程序；只比名稱，不查路徑。用於 ACT 連動設定前
/// 確認所有 ACT 都已關閉：ACT 結束時以記憶體中的設定覆寫設定檔，無從得知哪個 ACT 讀的是哪份設定檔。
pub(crate) fn any_named(names: &HashSet<String>) -> AppResult<bool> {
    Ok(processes()?.iter().any(|p| names.contains(&p.name.to_lowercase())))
}

/// 映像名稱（小寫）在 `names` 中的程序，是否有任一的完整映像路徑（[`comparable`] 形式）符合 `matches`。
/// 查不到或解析不了映像路徑的同名程序算符合（見模組說明）。
fn any_image_matches(names: &HashSet<String>, matches: impl Fn(&str) -> bool) -> AppResult<bool> {
    for process in processes()? {
        if !names.contains(&process.name.to_lowercase()) {
            continue;
        }
        let hit = match image_path(process.pid) {
            Ok(Some(image)) => match fs::canonicalize(image) {
                Ok(image) => matches(&comparable(&image)),
                // 映像路徑解析不了（例如位於已中斷的網路磁碟）：無法確認，保守視為符合。
                Err(_) => true,
            },
            // 列舉後已結束。
            Ok(None) => false,
            Err(_) => true,
        };
        if hit {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 比對用的路徑形式：解析成最終路徑（展開 junction／symlink／8.3 短名、取實際大小寫），再經
/// [`comparable`]。路徑不存在即失敗。
pub(crate) fn normalize_path(path: &Path) -> AppResult<String> {
    let canonical = fs::canonicalize(path).map_err(|e| AppError::io(format!("解析路徑 {}", path.display()), e))?;
    Ok(comparable(&canonical))
}

/// 只做字面轉換：`\\?\UNC\` → `\\`、去掉 `\\?\`、`/` → `\`、去掉結尾分隔符、轉小寫。
fn comparable(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    let text = match text.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => text.strip_prefix(r"\\?\").unwrap_or(&text).to_string(),
    };
    text.trim_end_matches('\\').to_lowercase()
}

/// 目標映像（皆為 [`comparable`] 形式）。
struct Targets {
    game_exe: String,
    boot_dir: String,
}

impl Targets {
    fn new(root: &str) -> Self {
        Self {
            game_exe: format!(r"{root}\game\{GAME_EXE}"),
            boot_dir: format!(r"{root}\boot"),
        }
    }

    /// `boot\` 只算直接位於其下的 `.exe`（官方執行檔都在這一層，含台服 `FfxivLauncherTC.exe`）。
    fn matches(&self, image: &str) -> bool {
        image == self.game_exe
            || image
                .rsplit_once('\\')
                .is_some_and(|(dir, name)| dir == self.boot_dir && name.ends_with(".exe"))
    }
}

/// 可能是目標映像的名稱（小寫）：`ffxiv_dx11.exe` 與目前 `boot\` 下所有 `.exe`。
fn image_names(install: &Path) -> AppResult<HashSet<String>> {
    let mut names = HashSet::from([GAME_EXE.to_string()]);
    let entries = match fs::read_dir(install.join("boot")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(names),
        Err(e) => return Err(AppError::io("讀取 boot 目錄", e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| AppError::io("讀取 boot 目錄", e))?;
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name.ends_with(".exe") {
            names.insert(name);
        }
    }
    Ok(names)
}

struct ProcessEntry {
    pid: u32,
    /// 映像檔名（不含目錄），只用來篩出需要查路徑的程序。
    name: String,
}

/// 系統上所有程序（Toolhelp32 snapshot，含其他使用者與 session）。
fn processes() -> AppResult<Vec<ProcessEntry>> {
    // SAFETY: 只傳值參數；成功時回傳的 snapshot handle 交給 Win32Handle 獨占並在 Drop 關閉。
    // quality-allow: Win32 FFI，Toolhelp 沒有安全包裝
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .map_err(|e| win_error("列舉程序", e))?;
    let snapshot = Win32Handle::new(snapshot);
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut list = Vec::new();
    // SAFETY: snapshot 是有效的程序 snapshot；entry 是本函式存活中的區域變數且 dwSize 已設定。
    // quality-allow: Win32 FFI，Toolhelp 沒有安全包裝
    let mut step = unsafe { Process32FirstW(snapshot.get(), &mut entry) };
    loop {
        match step {
            Ok(()) => list.push(ProcessEntry {
                pid: entry.th32ProcessID,
                name: wide_until_nul(&entry.szExeFile),
            }),
            Err(e) if e.code() == HRESULT::from_win32(ERROR_NO_MORE_FILES.0) => return Ok(list),
            Err(e) => return Err(win_error("列舉程序", e)),
        }
        // SAFETY: 同 Process32FirstW。quality-allow: Win32 FFI，Toolhelp 沒有安全包裝
        step = unsafe { Process32NextW(snapshot.get(), &mut entry) };
    }
}

fn wide_until_nul(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// 程序的完整映像路徑（Win32 形式）；程序已結束回 `None`。
fn image_path(pid: u32) -> windows::core::Result<Option<PathBuf>> {
    // SAFETY: OpenProcess 只讀取值參數；成功時回傳的 handle 交給 Win32Handle 獨占並在 Drop 關閉。
    // quality-allow: Win32 FFI，OpenProcess 沒有安全包裝
    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(handle) => Win32Handle::new(handle),
        // 程序不存在（列舉後已結束）時 OpenProcess 回 ERROR_INVALID_PARAMETER。
        Err(e) if e.code() == HRESULT::from_win32(ERROR_INVALID_PARAMETER.0) => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut buf = vec![0u16; IMAGE_PATH_CAP as usize];
    let mut len = IMAGE_PATH_CAP;
    // SAFETY: handle 含 PROCESS_QUERY_LIMITED_INFORMATION；buf 可寫 len 個 UTF-16 單位，成功時 len
    // 更新為實際長度（不含結尾 0）。quality-allow: Win32 FFI，QueryFullProcessImageNameW 沒有安全包裝
    unsafe {
        QueryFullProcessImageNameW(handle.get(), PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
    }?;
    buf.truncate(len as usize);
    Ok(Some(PathBuf::from(OsString::from_wide(&buf))))
}

/// 從 Win32 API 取得、由持有者獨占的 handle；Drop 時關閉（本模組與 helper 共用）。
pub(super) struct Win32Handle(HANDLE);

impl Win32Handle {
    /// `handle` 必須是剛由 Win32 API 回傳的有效 handle，且之後不在別處關閉。
    pub(super) fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    pub(super) fn get(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Win32Handle {
    fn drop(&mut self) {
        // SAFETY: 依 new 的前提，self.0 由本型別獨占，只在這裡關閉一次。
        // quality-allow: Win32 FFI；CloseHandle 失敗無可補救且 Drop 無法回傳錯誤
        let _ = unsafe { CloseHandle(self.0) };
    }
}

pub(super) fn win_error(context: &str, e: windows::core::Error) -> AppError {
    AppError::Io {
        context: context.to_string(),
        message: e.message(),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;
    use std::process::{Child, Command, Stdio};

    /// 本測試程序內，啟動 [`FakeProcess`] 的測試與其他會走到 [`install_in_use`](super::install_in_use)
    /// （直接或經 `apply_patches`、`update_game`）的測試互斥：前者持有寫鎖、後者持有讀鎖，到測試結束。
    /// 程序啟動與結束的瞬間查它的映像路徑會失敗（實測 E_ACCESSDENIED、ERROR_GEN_FAILURE），判定依保守規則
    /// 算使用中；而每個安裝的判定都會查所有名為 `ffxiv_dx11.exe` 的程序，替身因此會讓並行測試中別的安裝也被
    /// 判成使用中。寫鎖須宣告在 tempdir 與替身之前（最後才放開，替身結束的瞬間仍在鎖內）。tokio 的鎖：async
    /// 測試以 `.read().await`／`.write().await` 跨 await 持有，同步測試用 `blocking_read`／`blocking_write`。
    pub(crate) static GAME_PROCESSES: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

    /// 以完整路徑啟動的遊戲替身：把 `waitfor.exe` 複製到 `image` 再啟動，等一個不會出現的訊號
    /// （最多 120 秒）。不用 `timeout.exe`：它的 stdin 不是主控台時立即結束（測試的 stdin 常是
    /// 管線）。Drop 時結束並回收程序，tempdir 之後才刪得掉 exe；宣告順序須在 tempdir 之後。
    pub(crate) struct FakeProcess(Child);

    impl FakeProcess {
        pub(crate) fn start(image: &Path) -> Self {
            let system_root = std::env::var_os("SystemRoot").expect("SystemRoot");
            let waitfor = Path::new(&system_root).join("System32").join("waitfor.exe");
            std::fs::create_dir_all(image.parent().expect("parent")).expect("mkdir");
            std::fs::copy(waitfor, image).expect("copy waitfor.exe");
            let signal = format!("xlm{}", uuid::Uuid::new_v4().simple());
            let child = Command::new(image)
                .args(["/t", "120", &signal])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("start fake process");
            Self(child)
        }
    }

    impl Drop for FakeProcess {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{FakeProcess, GAME_PROCESSES};
    use super::*;

    fn fake_install() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for sub in ["game", "boot"] {
            fs::create_dir_all(dir.path().join(sub)).expect("mkdir");
        }
        dir
    }

    #[test]
    fn idle_install_is_not_in_use() {
        let _processes = GAME_PROCESSES.blocking_read();
        let install = fake_install();
        assert!(!install_in_use(install.path()).unwrap());
    }

    #[test]
    fn game_started_by_full_path_is_in_use() {
        let _processes = GAME_PROCESSES.blocking_write();
        let install = fake_install();
        let _game = FakeProcess::start(&install.path().join("game").join(GAME_EXE));
        assert!(install_in_use(install.path()).unwrap());
    }

    #[test]
    fn boot_executable_started_by_full_path_is_in_use() {
        let _processes = GAME_PROCESSES.blocking_write();
        let install = fake_install();
        let _launcher = FakeProcess::start(&install.path().join("boot").join("FfxivLauncherTC.exe"));
        assert!(install_in_use(install.path()).unwrap());
    }

    #[test]
    fn same_name_in_another_install_is_not_in_use() {
        let _processes = GAME_PROCESSES.blocking_write();
        let install = fake_install();
        let other = fake_install();
        let _game = FakeProcess::start(&other.path().join("game").join(GAME_EXE));
        assert!(!install_in_use(install.path()).unwrap());
        assert!(install_in_use(other.path()).unwrap());
    }

    #[test]
    fn missing_install_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(install_in_use(&dir.path().join("missing")).is_err());
    }

    /// 替身的名稱與遊戲執行檔不同，不影響其他測試的判定，不必取 GAME_PROCESSES。
    #[test]
    fn exe_running_matches_full_path_not_name() {
        const ACT_EXE: &str = "Advanced Combat Tracker.exe";
        let dir = tempfile::tempdir().expect("tempdir");
        let started = dir.path().join("a").join(ACT_EXE);
        let other = dir.path().join("b").join(ACT_EXE);
        fs::create_dir_all(other.parent().expect("parent")).expect("mkdir");
        fs::write(&other, b"").expect("write");
        assert!(!exe_running(&other).unwrap());
        let _act = FakeProcess::start(&started);
        assert!(exe_running(&started).unwrap());
        assert!(!exe_running(&other).unwrap());
    }

    #[test]
    fn target_rule() {
        let t = Targets::new(r"c:\games\ffxiv");
        assert!(t.matches(r"c:\games\ffxiv\game\ffxiv_dx11.exe"));
        assert!(t.matches(r"c:\games\ffxiv\boot\ffxivboot64.exe"));
        assert!(t.matches(r"c:\games\ffxiv\boot\ffxivlaunchertc.exe"));
        assert!(!t.matches(r"c:\games\ffxiv\boot\sub\ffxivboot64.exe"));
        assert!(!t.matches(r"c:\games\ffxiv\boot\ffxivboot.ver"));
        assert!(!t.matches(r"c:\games\ffxiv\game\other.exe"));
        assert!(!t.matches(r"c:\games\ffxiv\ffxiv_dx11.exe"));
        assert!(!t.matches(r"c:\games\ffxiv tc\game\ffxiv_dx11.exe"));
    }

    #[test]
    fn comparable_strips_verbatim_prefix_and_case() {
        assert_eq!(comparable(Path::new(r"\\?\C:\Games\FFXIV\")), r"c:\games\ffxiv");
        assert_eq!(comparable(Path::new(r"\\?\UNC\Server\Share\X")), r"\\server\share\x");
        assert_eq!(comparable(Path::new("C:/Games/FFXIV")), r"c:\games\ffxiv");
    }

    #[test]
    fn normalize_ignores_case_and_separators() {
        let install = fake_install();
        let text = install.path().to_string_lossy().to_uppercase().replace('\\', "/");
        let spelled = PathBuf::from(format!("{text}/"));
        assert_eq!(normalize_path(&spelled).unwrap(), normalize_path(install.path()).unwrap());
    }
}
