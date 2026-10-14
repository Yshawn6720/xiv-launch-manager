//! 設定檔與資料目錄。`config.json` 不含任何 secret（secret 在 `secrets.rs`）。

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::WIN32_ERROR;
use windows::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::core::HSTRING;

use crate::error::{AppError, AppResult};
use crate::region::{GlobalLanguage, Region};

/// 本程式暫存檔與暫存資料夾的結尾（[`write_replace`] 的 `<檔名>.<uuid>.xlm-tmp`、雙開 ACT 的暫存資料夾）。
pub const TEMP_SUFFIX: &str = ".xlm-tmp";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default)]
    pub games: GamePaths,
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub profiles: Vec<Profile>,
    /// ACT 設定（國際服、繁中服各一組）；舊設定檔沒有此欄位，視為未設定。
    #[serde(default)]
    pub act: ActConfig,
    /// 設定分頁「介面」；舊設定檔沒有此欄位（或缺其中一項）時用預設值。
    #[serde(default)]
    pub ui: UiSettings,
}

/// 設定分頁「介面」的兩個選項。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiSettings {
    #[serde(default)]
    pub theme: Theme,
    #[serde(default)]
    pub after_launch: AfterLaunch,
}

/// 介面主題：跟隨系統（預設）或強制深色、淺色。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Theme {
    #[default]
    System,
    Dark,
    Light,
}

/// 本程式成功啟動遊戲後主視窗怎麼做：不動作（預設）、最小化、縮到系統匣。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AfterLaunch {
    #[default]
    Keep,
    Minimize,
    Tray,
}

/// ACT（Advanced Combat Tracker）設定，依區服各一組：雙開時國際服、繁中服各用一個 ACT（各自固定
/// FFXIV 插件的 Region）；兩組指向同一個執行檔即單開。讀取時接受 2026-10-14 前的單一設定形狀
/// （`{path, launchWithGame}`），遷移成繁中服那一組、國際服未設定；寫回一律是依區服的形狀。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(from = "ActConfigFile")]
pub struct ActConfig {
    pub global: ActSettings,
    pub tw: ActSettings,
}

impl ActConfig {
    pub fn get(&self, region: Region) -> &ActSettings {
        match region {
            Region::Global => &self.global,
            Region::Tw => &self.tw,
        }
    }

    pub fn get_mut(&mut self, region: Region) -> &mut ActSettings {
        match region {
            Region::Global => &mut self.global,
            Region::Tw => &mut self.tw,
        }
    }
}

/// 設定檔中 `act` 可能的形狀；依序嘗試。兩種形狀都拒絕未知欄位：舊形狀不會被誤當成空的目前形狀，
/// 損壞的內容也不會被當成空設定（讀設定失敗，不覆寫使用者的檔案）。
#[derive(Deserialize)]
#[serde(untagged)]
enum ActConfigFile {
    Current(CurrentActConfig),
    Legacy(ActSettings),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CurrentActConfig {
    #[serde(default)]
    global: ActSettings,
    #[serde(default)]
    tw: ActSettings,
}

impl From<ActConfigFile> for ActConfig {
    fn from(file: ActConfigFile) -> Self {
        match file {
            ActConfigFile::Current(CurrentActConfig { global, tw }) => Self { global, tw },
            ActConfigFile::Legacy(tw) => Self {
                global: ActSettings::default(),
                tw,
            },
        }
    }
}

/// 一組 ACT 設定：只管執行檔路徑與是否隨該區服的遊戲開啟，不管 ACT 版本與 FFXIV 插件。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActSettings {
    /// ACT 執行檔的完整路徑；儲存時驗證過（`act::apply_settings`），之後執行檔仍可能被移走。
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// 登入該區服的遊戲後自動開啟 ACT；開啟時 `path` 必有值。
    #[serde(default)]
    pub launch_with_game: bool,
}

/// 各區服的遊戲安裝根目錄（含 `boot/`、`game/` 的那一層）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GamePaths {
    pub global: Option<PathBuf>,
    pub tw: Option<PathBuf>,
}

impl GamePaths {
    pub fn get(&self, region: Region) -> Option<&Path> {
        match region {
            Region::Global => self.global.as_deref(),
            Region::Tw => self.tw.as_deref(),
        }
    }

    pub fn set(&mut self, region: Region, path: Option<PathBuf>) {
        match region {
            Region::Global => self.global = path,
            Region::Tw => self.tw = path,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: String,
    pub name: String,
    pub region: Region,
    pub username: String,
    pub otp: OtpMode,
    /// 綁定的衛月 profile；`None` 表示此帳號不載入衛月。
    pub profile_id: Option<String>,
    /// 只對國際服有意義；台服客戶端沒有語言參數。
    #[serde(default)]
    pub language: GlobalLanguage,
    /// `true`＝密碼存在 Credential Manager；`false`＝每次啟動手動輸入、不保存。
    /// 舊設定檔沒有此欄位，視為 `true`（當時一律儲存）。
    #[serde(default = "default_true")]
    pub remember_password: bool,
    /// 額外遊戲參數，空白分隔的 `key=value`（protocols §2.6）；儲存時已驗證並正規化。
    #[serde(default)]
    pub extra_args: String,
    /// 「ACT 跟這個帳號」：登入時寫 ACT 連動指令檔、並依該區服的 ACT 設定自動開啟（protocols §6.2）。
    /// 同服多開時小號取消，免得把 ACT 從主號搶走。舊設定檔沒有此欄位，視為 `true`（當時一律連動）。
    #[serde(default = "default_true")]
    pub act_follow: bool,
}

pub(crate) fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OtpMode {
    None,
    /// 每次啟動由使用者輸入。
    Manual,
    /// 以 Credential Manager 中的金鑰本機產生 TOTP。
    Auto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub channel: DalamudChannel,
    pub injection_delay_ms: u32,
    /// 沿用的外部衛月資料夾（舊啟動器）；`None`＝本程式管理的 `profiles/<id>/`。
    #[serde(default)]
    pub data_dir: Option<PathBuf>,
}

impl Profile {
    pub fn region(&self) -> Region {
        self.channel.region()
    }
}

/// 衛月來源與版本選擇。來源決定區服（protocols §1.5、§2.5）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "camelCase")]
pub enum DalamudChannel {
    /// goatcorp 當前 release（只配國際服）。
    Goatcorp,
    /// yanmucorp（只配台服）。
    Yanmucorp { version: YanmucorpVersion },
}

impl DalamudChannel {
    pub fn region(&self) -> Region {
        match self {
            DalamudChannel::Goatcorp => Region::Global,
            DalamudChannel::Yanmucorp { .. } => Region::Tw,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum YanmucorpVersion {
    Latest,
    Pinned { tag: String },
}

/// `%APPDATA%\xiv-launch-manager` 底下的固定配置（主 note「資料配置」）。
#[derive(Debug, Clone)]
pub struct AppPaths {
    root: PathBuf,
}

impl AppPaths {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn from_env() -> AppResult<Self> {
        let appdata = std::env::var_os("APPDATA").ok_or_else(|| AppError::NotFound {
            what: "APPDATA 環境變數".into(),
        })?;
        Ok(Self::new(PathBuf::from(appdata).join("xiv-launch-manager")))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.json")
    }
    pub fn running_file(&self) -> PathBuf {
        self.root.join("state").join("running.json")
    }
    pub fn accounts_running_file(&self) -> PathBuf {
        self.root.join("state").join("accounts_running.json")
    }
    /// `state/patch_dirty.json`：遊戲更新的 dirty 標記（`patch::dirty`）。
    pub fn patch_dirty_file(&self) -> PathBuf {
        self.root.join("state").join("patch_dirty.json")
    }
    /// `state/act_target.json`：ACT 連動外掛讀的各區服遊戲程序（`act_bridge`、protocols §6.2）。
    pub fn act_target_file(&self) -> PathBuf {
        self.root.join("state").join("act_target.json")
    }
    /// `patches/`：遊戲 patch 下載與國際服 helper 的 job 目錄（皆由一般權限的本程式建立）。
    pub fn patches_dir(&self) -> PathBuf {
        self.root.join("patches")
    }
    /// `dalamud/<source>/<version>/`
    pub fn dalamud_dir(&self, source: &str, version: &str) -> PathBuf {
        self.root.join("dalamud").join(source).join(version)
    }
    /// `runtime/<dotnet_version>/`
    pub fn runtime_dir(&self, dotnet_version: &str) -> PathBuf {
        self.root.join("runtime").join(dotnet_version)
    }
    /// `assets/<source>/<version>/`
    pub fn assets_dir(&self, source: &str, version: &str) -> PathBuf {
        self.root.join("assets").join(source).join(version)
    }
    /// `profiles/<profile_id>/`
    pub fn profile_dir(&self, profile_id: &str) -> PathBuf {
        self.root.join("profiles").join(profile_id)
    }
    /// 下載與解壓的暫存區；與正式目錄同一磁碟，rename 才是原子的。
    pub fn staging_dir(&self) -> PathBuf {
        self.root.join("staging")
    }
}

/// 讀設定；檔案不存在回傳空設定，格式錯誤回報錯誤（不覆寫使用者的檔案）。
pub fn load_config(paths: &AppPaths) -> AppResult<Config> {
    let file = paths.config_file();
    match fs::read(&file) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| AppError::Io {
            context: "解析 config.json".into(),
            message: e.to_string(),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(AppError::io("讀取 config.json", e)),
    }
}

pub fn save_config(paths: &AppPaths, config: &Config) -> AppResult<()> {
    write_json_atomic(&paths.config_file(), config)
}

/// 序列化成排版過的 JSON，以 [`write_replace`] 寫入：回傳時內容與檔名都已落盤；中斷或斷電後 `file` 不是舊內容
/// 就是新內容，不會留下半個 JSON。config.json 與 `state/` 下的狀態檔都經這裡。
pub fn write_json_atomic<T: Serialize>(file: &Path, value: &T) -> AppResult<()> {
    let dir = file.parent().ok_or_else(|| AppError::Io {
        context: "寫入 JSON".into(),
        message: format!("{} 沒有上層目錄", file.display()),
    })?;
    fs::create_dir_all(dir).map_err(|e| AppError::io("建立目錄", e))?;
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| AppError::Io {
        context: "序列化 JSON".into(),
        message: e.to_string(),
    })?;
    write_replace(file, &bytes).map_err(|e| AppError::io(format!("寫入 {}", file.display()), e))
}

/// 先寫同資料夾的唯一暫存檔並 `sync_all`，再以 [`replace_write_through`] 取代 `target`；失敗時刪掉暫存檔，
/// `target` 不變。
pub(crate) fn write_replace(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = with_suffix(target, &format!(".{}{TEMP_SUFFIX}", uuid::Uuid::new_v4().simple()));
    let result = write_new(&tmp, bytes).and_then(|()| replace_write_through(&tmp, target));
    if result.is_err() {
        // quality-allow: 暫存檔清理是 best-effort，刪不掉只留下唯一名稱的暫存檔，原檔不變
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// 建立新檔（已存在就失敗）、寫入並 `sync_all`。
pub(crate) fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// 以 `from` 原子取代 `to`：`MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)`，改名寫入
/// 磁碟後才返回（`fs::rename` 返回時改名可能還只在快取，斷電會遺失）。不 flush `from` 的資料：呼叫端須先
/// 對它 `sync_all`。[`write_replace`] 與 `.ver` 的 commit 點（`patch::zipatch`）都經這裡。
pub(crate) fn replace_write_through(from: &Path, to: &Path) -> io::Result<()> {
    let from = HSTRING::from(from);
    let to = HSTRING::from(to);
    // SAFETY: 兩個路徑都是以 0 結尾（HSTRING 保證）、在呼叫期間存活的 UTF-16 字串。
    // quality-allow: Win32 FFI，MoveFileExW 沒有安全包裝
    let moved = unsafe { MoveFileExW(&from, &to, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) };
    // HRESULT_FROM_WIN32 還原成 Win32 錯誤碼，`io::Error` 的 kind 與訊息才與 std 的檔案操作一致。
    moved.map_err(|e| match WIN32_ERROR::from_error(&e) {
        Some(code) => io::Error::from_raw_os_error(code.0 as i32),
        None => e.into(),
    })
}

/// 在檔名後直接接上 `suffix`（`a.xml` → `a.xml.xlm-backup`）。
pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_through_atomic_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().to_path_buf());
        let mut config = Config::default();
        config.profiles.push(Profile {
            id: "p1".into(),
            name: "台服".into(),
            channel: DalamudChannel::Yanmucorp {
                version: YanmucorpVersion::Pinned {
                    tag: "26-10-03-01".into(),
                },
            },
            injection_delay_ms: 0,
            data_dir: None,
        });
        save_config(&paths, &config).expect("save");
        let loaded = load_config(&paths).expect("load");
        assert_eq!(loaded.profiles[0].channel, config.profiles[0].channel);
        assert_eq!(loaded.profiles[0].region(), Region::Tw);
        let names: Vec<_> = fs::read_dir(dir.path())
            .expect("list")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["config.json"], "沒有留下暫存檔");
    }

    #[test]
    fn corrupt_config_is_an_error_not_an_empty_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().to_path_buf());
        fs::write(paths.config_file(), b"{not json").expect("write");
        assert!(load_config(&paths).is_err());
    }

    #[test]
    fn legacy_account_defaults_remember_password_and_act_follow_to_true() {
        let legacy = serde_json::json!({
            "id": "a",
            "name": "n",
            "region": "global",
            "username": "u",
            "otp": "none",
            "profileId": null
        });
        let account: Account = serde_json::from_value(legacy).expect("legacy account");
        assert!(account.remember_password);
        assert!(account.act_follow);

        let explicit: Account = serde_json::from_value(serde_json::json!({
            "id": "a",
            "name": "n",
            "region": "global",
            "username": "u",
            "otp": "none",
            "profileId": null,
            "rememberPassword": false
        }))
        .expect("explicit account");
        assert!(!explicit.remember_password);
        assert_eq!(serde_json::to_value(&explicit).expect("json")["rememberPassword"], false);
    }

    #[test]
    fn legacy_config_without_act_loads_with_act_unset() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().to_path_buf());
        let legacy = serde_json::json!({
            "games": { "global": null, "tw": r"C:\Games\FFXIV TC" },
            "accounts": [],
            "profiles": []
        });
        fs::write(paths.config_file(), legacy.to_string()).expect("write");
        let config = load_config(&paths).expect("legacy config");
        assert_eq!(config.games.tw.as_deref(), Some(Path::new(r"C:\Games\FFXIV TC")));
        assert_eq!(config.act.tw.path, None);
        assert_eq!(config.act.global.path, None);
        assert!(!config.act.tw.launch_with_game);
    }

    #[test]
    fn legacy_config_without_ui_uses_defaults_and_saves_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().to_path_buf());
        fs::write(paths.config_file(), serde_json::json!({ "games": {} }).to_string()).expect("write");
        let config = load_config(&paths).expect("legacy config");
        assert_eq!(config.ui.theme, Theme::System);
        assert_eq!(config.ui.after_launch, AfterLaunch::Keep);

        save_config(&paths, &config).expect("save");
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(paths.config_file()).expect("read")).expect("json");
        assert_eq!(saved["ui"], serde_json::json!({ "theme": "system", "afterLaunch": "keep" }));
    }

    #[test]
    fn ui_settings_missing_one_field_keep_the_other() {
        let ui: UiSettings = serde_json::from_value(serde_json::json!({ "theme": "dark" })).expect("theme only");
        assert_eq!(ui, UiSettings { theme: Theme::Dark, after_launch: AfterLaunch::Keep });
        let ui: UiSettings =
            serde_json::from_value(serde_json::json!({ "afterLaunch": "tray" })).expect("after launch only");
        assert_eq!(ui, UiSettings { theme: Theme::System, after_launch: AfterLaunch::Tray });
    }

    #[test]
    fn single_act_settings_migrate_to_tw_and_save_per_region() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().to_path_buf());
        let exe = r"C:\Program Files (x86)\Advanced Combat Tracker\Advanced Combat Tracker.exe";
        let legacy = serde_json::json!({
            "games": {},
            "act": { "path": exe, "launchWithGame": true }
        });
        fs::write(paths.config_file(), legacy.to_string()).expect("write");
        let config = load_config(&paths).expect("legacy act");
        assert_eq!(config.act.tw.path.as_deref(), Some(Path::new(exe)));
        assert!(config.act.tw.launch_with_game);
        assert_eq!(config.act.global.path, None);
        assert!(!config.act.global.launch_with_game);

        // 寫回後是依區服的形狀，再讀一次不變。
        save_config(&paths, &config).expect("save");
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(paths.config_file()).expect("read")).expect("json");
        assert_eq!(
            saved["act"],
            serde_json::json!({
                "global": { "path": null, "launchWithGame": false },
                "tw": { "path": exe, "launchWithGame": true }
            })
        );
        let reloaded = load_config(&paths).expect("reload");
        assert_eq!(reloaded.act.tw.path.as_deref(), Some(Path::new(exe)));
        assert!(reloaded.act.tw.launch_with_game);
        assert_eq!(reloaded.act.global.path, None);
    }

    #[test]
    fn per_region_act_settings_load_as_is() {
        let act: ActConfig = serde_json::from_value(serde_json::json!({
            "global": { "path": r"D:\ACT Global\Advanced Combat Tracker.exe", "launchWithGame": true }
        }))
        .expect("per-region act");
        assert_eq!(
            act.get(Region::Global).path.as_deref(),
            Some(Path::new(r"D:\ACT Global\Advanced Combat Tracker.exe"))
        );
        assert!(act.get(Region::Global).launch_with_game);
        assert_eq!(act.get(Region::Tw).path, None);
    }
}
