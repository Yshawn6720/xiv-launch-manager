//! ACT（Advanced Combat Tracker）：執行檔路徑的驗證與偵測、執行判定、開啟，以及可攜式 ACT 的判定與設定檔
//! 位置（protocols §6.1）。國際服、繁中服各一組設定（`store::ActConfig`），本模組只處理單一組。ACT 連動外掛
//! 見 `act_bridge`。開啟 ACT 不提權：ACT 的 manifest 要求系統管理員時由 Windows 跳 UAC。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Serialize;
use windows::Win32::Foundation::ERROR_CANCELLED;
use windows::Win32::UI::Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HRESULT, HSTRING, PCWSTR, w};

use crate::error::{AppError, AppResult};
use crate::patch::running;
use crate::store::ActSettings;

/// 官方安裝程式的資料夾與執行檔名稱。
const INSTALL_DIR: &str = "Advanced Combat Tracker";
pub(crate) const EXE_NAME: &str = "Advanced Combat Tracker.exe";
/// ACT 設定檔所在的子資料夾（ACT 的 AppDataFolder 下）；檔名見 [`config_file_name`]。
const CONFIG_DIR: &str = "Config";
/// 解除安裝登錄中 DisplayName 含此字串（不分大小寫）的項目視為 ACT。
const DISPLAY_NAME_LOWER: &str = "advanced combat tracker";

/// ACT 目前的狀態（畫面顯示用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ActStatus {
    /// 尚未設定執行檔路徑。
    NotConfigured,
    /// 設定的執行檔已不存在。
    Missing,
    Running,
    Stopped,
}

/// [`start`] 的結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ActLaunch {
    Started,
    /// 已在執行，沒有再開一個。
    AlreadyRunning,
}

/// 儲存與開啟前的驗證：完整路徑（含磁碟機代號或 UNC）、副檔名 `.exe`、檔案存在。
pub fn validate_exe(path: &Path) -> AppResult<()> {
    let reason = if !path.is_absolute() {
        "不是完整路徑"
    } else if !has_exe_extension(path) {
        "不是 .exe 檔"
    } else if !path.is_file() {
        "找不到檔案"
    } else {
        return Ok(());
    };
    Err(AppError::InvalidActPath {
        reason: format!("{reason}（{}）", path.display()),
    })
}

fn has_exe_extension(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe"))
}

/// 套用使用者儲存的 ACT 設定：與已存不同的路徑須通過 [`validate_exe`]（沿用已存的路徑不重驗：執行檔不見
/// 時仍能關掉自動開啟，畫面狀態另顯示「找不到」）；開啟自動開啟時必須有路徑。失敗時不改動 `settings`。
pub fn apply_settings(settings: &mut ActSettings, path: Option<PathBuf>, launch_with_game: bool) -> AppResult<()> {
    match &path {
        Some(p) if settings.path.as_ref() != Some(p) => validate_exe(p)?,
        Some(_) => {}
        None if launch_with_game => return Err(AppError::ActNotConfigured),
        None => {}
    }
    *settings = ActSettings { path, launch_with_game };
    Ok(())
}

/// 系統安裝的 ACT：解除安裝登錄與預設安裝位置的候選中，通過 [`validate_exe`] 的執行檔（登錄在前；同一個執行檔
/// 可能出現多次，由呼叫端去重）。找不到為空。
pub fn detect() -> Vec<PathBuf> {
    let defaults = default_candidates(std::env::var_os("ProgramFiles(x86)"), std::env::var_os("ProgramFiles"));
    valid_candidates(registry_candidates().into_iter().chain(defaults))
}

fn valid_candidates(candidates: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    candidates.into_iter().filter(|p| validate_exe(p).is_ok()).collect()
}

/// HKLM（64／32 位元視圖）與 HKCU 解除安裝登錄中，ACT 項目指向的執行檔候選。讀不到的機碼或值略過。
fn registry_candidates() -> Vec<PathBuf> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    let roots = [
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall"),
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall"),
        (HKEY_CURRENT_USER, r"Software\Microsoft\Windows\CurrentVersion\Uninstall"),
    ];
    let mut found = Vec::new();
    for (hive, path) in roots {
        let Ok(list) = RegKey::predef(hive).open_subkey(path) else {
            continue;
        };
        for name in list.enum_keys().flatten() {
            let Ok(entry) = list.open_subkey(&name) else {
                continue;
            };
            let Ok(display_name) = entry.get_value::<String, _>("DisplayName") else {
                continue;
            };
            let location = entry.get_value::<String, _>("InstallLocation").ok();
            let icon = entry.get_value::<String, _>("DisplayIcon").ok();
            found.extend(entry_candidates(&display_name, location.as_deref(), icon.as_deref()));
        }
    }
    found
}

/// 一個解除安裝項目指向的執行檔候選（未驗證存在）：DisplayName 不含 ACT 名稱就沒有。登錄值只用來
/// 定位安裝資料夾——InstallLocation 是資料夾（是檔案時取其所在資料夾），DisplayIcon 取圖示檔所在
/// 資料夾——候選一律是該資料夾下的 [`EXE_NAME`]。DisplayIcon 常指向 `Uninstall-ACT.exe`，不能直接當執行檔。
fn entry_candidates(display_name: &str, install_location: Option<&str>, display_icon: Option<&str>) -> Vec<PathBuf> {
    if !display_name.to_ascii_lowercase().contains(DISPLAY_NAME_LOWER) {
        return Vec::new();
    }
    let location = install_location
        .map(|s| s.trim().trim_matches('"'))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .and_then(|path| if has_exe_extension(&path) { path.parent().map(Path::to_path_buf) } else { Some(path) });
    let icon_dir = display_icon.and_then(icon_file).and_then(|file| file.parent().map(Path::to_path_buf));
    let mut dirs: Vec<PathBuf> = location.into_iter().chain(icon_dir).collect();
    dirs.dedup();
    dirs.into_iter().map(|dir| dir.join(EXE_NAME)).collect()
}

/// DisplayIcon 的值（`"C:\…\x.exe",0`、`C:\…\x.exe,0` 或 `C:\…\x.exe`）去掉引號與圖示索引後的路徑。
fn icon_file(value: &str) -> Option<PathBuf> {
    let value = value.trim();
    let path = match value.strip_prefix('"') {
        Some(quoted) => quoted.split_once('"')?.0,
        None => match value.rsplit_once(',') {
            Some((path, index)) if is_icon_index(index) => path,
            _ => value,
        },
    };
    let path = path.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// 圖示索引：可帶負號的整數（負數為資源 id）。
fn is_icon_index(text: &str) -> bool {
    let digits = text.trim().strip_prefix('-').unwrap_or(text.trim());
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// `%ProgramFiles(x86)%`、`%ProgramFiles%` 下的預設安裝位置（依此順序）。
fn default_candidates(program_files_x86: Option<OsString>, program_files: Option<OsString>) -> Vec<PathBuf> {
    [program_files_x86, program_files]
        .into_iter()
        .flatten()
        .map(|dir| PathBuf::from(dir).join(INSTALL_DIR).join(EXE_NAME))
        .collect()
}

/// ACT 設定檔名：`<執行檔名去副檔名>.config.xml`（ACT 以自己的程序映像名稱命名，protocols §6.1）。
pub(crate) fn config_file_name(exe: &Path) -> String {
    let stem = exe.file_stem().map(|s| s.to_string_lossy()).unwrap_or_default();
    format!("{stem}.config.xml")
}

/// 可攜式 ACT（protocols §6.1）：exe 所在資料夾內有 `Config\<exe 名>.config.xml`。主安裝的設定在
/// `%APPDATA%`，安裝資料夾沒有這個檔。
pub fn is_portable(exe: &Path) -> bool {
    exe.parent()
        .is_some_and(|dir| dir.join(CONFIG_DIR).join(config_file_name(exe)).is_file())
}

/// 這個 ACT 讀寫的設定檔：可攜式為 exe 資料夾的 `Config\…`，否則 `%APPDATA%\Advanced Combat Tracker\Config\…`
/// （ACT 的 AppDataFolder，protocols §6.1）。`appdata` 是 `%APPDATA%`。
pub fn config_file(exe: &Path, appdata: &Path) -> PathBuf {
    let data_dir = match exe.parent() {
        Some(dir) if is_portable(exe) => dir.to_path_buf(),
        _ => appdata.join(INSTALL_DIR),
    };
    data_dir.join(CONFIG_DIR).join(config_file_name(exe))
}

/// 開啟 ACT 的參數（protocols §6.1）：一律 `-skipmutex`（雙開時另一個 ACT 已在執行）；可攜式另加
/// `-portable`（設定改讀 exe 資料夾的 `Config\`）。
fn launch_parameters(exe: &Path) -> &'static str {
    if is_portable(exe) {
        "-skipmutex -portable"
    } else {
        "-skipmutex"
    }
}

/// 兩個路徑是否指向同一處：都解析得到時比最終路徑（展開連結、不分大小寫），否則比字面（不分大小寫、
/// `/` 視同 `\`）。用於判定兩組設定是否同一個 ACT（單開）或同一個資料夾。
pub fn same_location(a: &Path, b: &Path) -> bool {
    match (running::normalize_path(a), running::normalize_path(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => literal_key(a) == literal_key(b),
    }
}

fn literal_key(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// 設定的 ACT 狀態；執行判定以完整映像路徑比對（`patch::running`），會列舉系統程序。
pub fn status(settings: &ActSettings) -> AppResult<ActStatus> {
    let Some(exe) = settings.path.as_deref() else {
        return Ok(ActStatus::NotConfigured);
    };
    if !exe.is_file() {
        return Ok(ActStatus::Missing);
    }
    Ok(if running::exe_running(exe)? {
        ActStatus::Running
    } else {
        ActStatus::Stopped
    })
}

/// 開啟 ACT：驗證路徑，已在執行就不再開；否則以 `ShellExecuteExW` 開啟。UAC 對話框期間會阻塞，
/// 所以放到 blocking 執行緒。
pub async fn start(path: Option<PathBuf>) -> AppResult<ActLaunch> {
    let exe = path.ok_or(AppError::ActNotConfigured)?;
    tokio::task::spawn_blocking(move || start_blocking(&exe))
        .await
        .map_err(|e| AppError::ActLaunchFailed { reason: e.to_string() })?
}

fn start_blocking(exe: &Path) -> AppResult<ActLaunch> {
    validate_exe(exe)?;
    if running::exe_running(exe)? {
        return Ok(ActLaunch::AlreadyRunning);
    }
    shell_open(exe)?;
    Ok(ActLaunch::Started)
}

/// `ShellExecuteExW`（verb `open`，參數見 [`launch_parameters`]，工作目錄＝exe 所在目錄）：ACT 的 manifest
/// 要求系統管理員時由 Windows 跳 UAC，被拒為 `ERROR_CANCELLED`。不等 ACT 結束，也不取程序 handle。
fn shell_open(exe: &Path) -> AppResult<()> {
    let Some(dir) = exe.parent() else {
        return Err(AppError::InvalidActPath {
            reason: format!("沒有上層目錄（{}）", exe.display()),
        });
    };
    let file = HSTRING::from(exe);
    let parameters = HSTRING::from(launch_parameters(exe));
    let directory = HSTRING::from(dir);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        // NOASYNC：呼叫端是沒有訊息迴圈的背景執行緒，要求呼叫返回前完成啟動。
        fMask: SEE_MASK_NOASYNC,
        lpVerb: w!("open"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        lpDirectory: PCWSTR(directory.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: info 的字串指標指向本函式持有、以 0 結尾（HSTRING 保證）且呼叫期間存活的緩衝區；cbSize 已設定；
    // 沒有 SEE_MASK_NOCLOSEPROCESS，不會回傳需要關閉的 handle。quality-allow: Win32 FFI，ShellExecuteExW 沒有安全包裝
    match unsafe { ShellExecuteExW(&mut info) } {
        Ok(()) => Ok(()),
        Err(e) if e.code() == HRESULT::from_win32(ERROR_CANCELLED.0) => Err(AppError::ActLaunchFailed {
            reason: "未允許系統管理員權限".into(),
        }),
        Err(e) => Err(AppError::ActLaunchFailed { reason: e.message() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invalid_reason(path: &Path) -> String {
        match validate_exe(path) {
            Err(AppError::InvalidActPath { reason }) => reason,
            other => panic!("預期 InvalidActPath，得到 {other:?}"),
        }
    }

    #[test]
    fn apply_settings_validates_new_paths_and_requires_path_for_auto_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join(EXE_NAME);
        std::fs::write(&exe, b"").expect("write exe");
        let mut settings = ActSettings::default();

        let err = apply_settings(&mut settings, Some(PathBuf::from(EXE_NAME)), false);
        assert!(matches!(err, Err(AppError::InvalidActPath { .. })));
        let err = apply_settings(&mut settings, None, true);
        assert!(matches!(err, Err(AppError::ActNotConfigured)));
        assert_eq!(settings.path, None);
        assert!(!settings.launch_with_game);

        apply_settings(&mut settings, Some(exe.clone()), true).expect("valid exe");
        assert_eq!(settings.path.as_deref(), Some(exe.as_path()));
        assert!(settings.launch_with_game);

        // 已存的執行檔不見了：沿用同一路徑仍能關掉自動開啟；換成另一個不存在的路徑則拒絕。
        std::fs::remove_file(&exe).expect("remove exe");
        apply_settings(&mut settings, Some(exe.clone()), false).expect("same missing path");
        assert!(!settings.launch_with_game);
        let other = dir.path().join("other.exe");
        assert!(apply_settings(&mut settings, Some(other), false).is_err());
        assert_eq!(settings.path.as_deref(), Some(exe.as_path()));
    }

    #[test]
    fn validate_rejects_relative_missing_and_non_exe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join(EXE_NAME);
        std::fs::write(&exe, b"").expect("write exe");
        let text = dir.path().join("readme.txt");
        std::fs::write(&text, b"").expect("write txt");

        assert!(validate_exe(&exe).is_ok());
        assert!(invalid_reason(Path::new(EXE_NAME)).starts_with("不是完整路徑"));
        assert!(invalid_reason(Path::new(r"C:Advanced Combat Tracker.exe")).starts_with("不是完整路徑"));
        assert!(invalid_reason(&dir.path().join("missing.exe")).starts_with("找不到檔案"));
        assert!(invalid_reason(&text).starts_with("不是 .exe 檔"));
        // 名為 .exe 的資料夾不是執行檔。
        let folder = dir.path().join("folder.exe");
        std::fs::create_dir(&folder).expect("mkdir");
        assert!(invalid_reason(&folder).starts_with("找不到檔案"));
    }

    #[test]
    fn icon_file_strips_quotes_and_index() {
        let want = Some(PathBuf::from(r"C:\Program Files (x86)\Advanced Combat Tracker\Advanced Combat Tracker.exe"));
        assert_eq!(
            icon_file(r#""C:\Program Files (x86)\Advanced Combat Tracker\Advanced Combat Tracker.exe",0"#),
            want
        );
        assert_eq!(icon_file(r"C:\Program Files (x86)\Advanced Combat Tracker\Advanced Combat Tracker.exe,-101"), want);
        assert_eq!(icon_file(r" C:\Program Files (x86)\Advanced Combat Tracker\Advanced Combat Tracker.exe "), want);
        // 逗號後不是數字：屬於路徑本身。
        assert_eq!(icon_file(r"C:\a,b\x.exe"), Some(PathBuf::from(r"C:\a,b\x.exe")));
        assert_eq!(icon_file(r#""""#), None);
        assert_eq!(icon_file("  "), None);
    }

    #[test]
    fn entry_candidates_follow_display_name_location_and_icon() {
        assert!(entry_candidates("Some Other App", Some(r"C:\ACT"), Some(r"C:\ACT\a.exe")).is_empty());
        assert_eq!(
            entry_candidates("Advanced Combat Tracker (remove only)", Some(r#""C:\ACT\""#), Some(r"D:\Other\icon.ico,0")),
            vec![PathBuf::from(r"C:\ACT\").join(EXE_NAME), PathBuf::from(r"D:\Other").join(EXE_NAME)]
        );
        // InstallLocation 指向檔案：取所在資料夾。
        assert_eq!(
            entry_candidates("advanced combat tracker", Some(r"D:\Tools\ACT.EXE"), None),
            vec![PathBuf::from(r"D:\Tools").join(EXE_NAME)]
        );
    }

    #[test]
    fn display_icon_pointing_at_uninstaller_yields_main_exe() {
        // 實機登錄（ACT 安裝程式只寫 DisplayIcon／UninstallString，沒有 InstallLocation）。
        assert_eq!(
            entry_candidates(
                "Advanced Combat Tracker (remove only)",
                None,
                Some(r#""C:\Program Files (x86)\Advanced Combat Tracker\Uninstall-ACT.exe",0"#),
            ),
            vec![PathBuf::from(r"C:\Program Files (x86)\Advanced Combat Tracker\Advanced Combat Tracker.exe")]
        );
    }

    #[test]
    fn default_candidates_prefer_x86_and_skip_unset() {
        assert_eq!(
            default_candidates(Some(r"C:\PF86".into()), Some(r"C:\PF".into())),
            vec![
                PathBuf::from(r"C:\PF86\Advanced Combat Tracker\Advanced Combat Tracker.exe"),
                PathBuf::from(r"C:\PF\Advanced Combat Tracker\Advanced Combat Tracker.exe"),
            ]
        );
        assert_eq!(
            default_candidates(None, Some(r"C:\PF".into())),
            vec![PathBuf::from(r"C:\PF\Advanced Combat Tracker\Advanced Combat Tracker.exe")]
        );
    }

    #[test]
    fn valid_candidates_keep_existing_exes_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let icon = dir.path().join("icon.ico");
        std::fs::write(&icon, b"").expect("write ico");
        let exe = dir.path().join(EXE_NAME);
        std::fs::write(&exe, b"").expect("write exe");
        let later = dir.path().join("later.exe");
        std::fs::write(&later, b"").expect("write later");
        let found = valid_candidates([dir.path().join("missing.exe"), icon, exe.clone(), later.clone()]);
        assert_eq!(found, vec![exe, later]);
        assert!(valid_candidates([dir.path().join("missing.exe")]).is_empty());
    }

    /// 可攜式 ACT 的資料夾布局：`<exe 資料夾>\Config\<exe 名>.config.xml`。
    fn make_portable(exe: &Path) {
        let config = exe.parent().expect("parent").join(CONFIG_DIR);
        std::fs::create_dir_all(&config).expect("mkdir Config");
        std::fs::write(config.join(config_file_name(exe)), b"<Config />").expect("write config");
    }

    #[test]
    fn launch_parameters_add_portable_only_with_config_file_beside_exe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join(EXE_NAME);
        assert_eq!(launch_parameters(&exe), "-skipmutex");
        // 只有 Config 資料夾、沒有設定檔：仍是主安裝。
        std::fs::create_dir(dir.path().join(CONFIG_DIR)).expect("mkdir");
        assert_eq!(launch_parameters(&exe), "-skipmutex");
        // 別的執行檔名的設定檔不算：ACT 以自己的映像名稱找設定檔。
        make_portable(&dir.path().join("ACTx86.exe"));
        assert_eq!(launch_parameters(&exe), "-skipmutex");
        make_portable(&exe);
        assert_eq!(launch_parameters(&exe), "-skipmutex -portable");
    }

    #[test]
    fn config_file_follows_portable_layout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join(EXE_NAME);
        let appdata = Path::new(r"C:\Users\u\AppData\Roaming");
        assert_eq!(
            config_file(&exe, appdata),
            appdata.join(r"Advanced Combat Tracker\Config\Advanced Combat Tracker.config.xml")
        );
        make_portable(&exe);
        assert_eq!(
            config_file(&exe, appdata),
            dir.path().join(r"Config\Advanced Combat Tracker.config.xml")
        );
    }

    #[test]
    fn same_location_ignores_case_and_separators() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join(EXE_NAME);
        std::fs::write(&exe, b"").expect("write exe");
        let upper = PathBuf::from(exe.to_string_lossy().to_uppercase());
        assert!(same_location(&exe, &upper));
        assert!(!same_location(&exe, &dir.path().join("other.exe")));
        // 不存在的路徑比字面。
        assert!(same_location(Path::new(r"D:\ACT\x.exe"), Path::new("d:/act/X.EXE")));
        assert!(!same_location(Path::new(r"D:\ACT\x.exe"), Path::new(r"D:\ACT2\x.exe")));
    }
}
