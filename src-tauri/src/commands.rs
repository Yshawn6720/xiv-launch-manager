//! 前端可呼叫的 commands。secret 只有寫入／刪除路徑，沒有任何 command 讀回 secret；
//! `preview_otp` 只回傳由金鑰算出的當下 OTP。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::{AppHandle, State};
use tokio::sync::Mutex;

use crate::act::{self, ActLaunch, ActStatus};
use crate::characters;
use crate::act_bridge::{self, ActChange, ActChoice, DualResult, LinkStatus};
use crate::act_plugin::{self, LatestCache, PluginVersions};
use crate::dalamud::{
    self, DetectedDalamudData, DownloadCancel, InstalledVersion, Progress, ProgressFn, ProfileInfo,
    ReleaseInfo,
};
use crate::error::{AppError, AppResult};
use crate::global::{self, ServerStatus};
use crate::launch::{self, LaunchOptions, LaunchOutcome};
use crate::main_window::{self, MainWindow};
use crate::patch::integrity::{self, IntegrityOutcome};
use crate::patch::update::{
    self as game_update, GameUpdateProgress, GameUpdateStatus, PatchState, PendingUpdates,
};
use crate::region::Region;
use crate::secrets::{self, SecretKind};
use crate::store::{
    self, Account, ActConfig, AfterLaunch, AppPaths, Config, DalamudChannel, GamePaths, Profile, Theme,
    UiSettings, YanmucorpVersion,
};
use crate::totp;
use crate::update::{self, UpdateProgress};

pub struct AppState {
    pub paths: AppPaths,
    /// 安裝衛月與啟動遊戲共用的 app-wide 鎖（主 note「程序與狀態」）。
    pub launch_lock: Mutex<()>,
    /// 設定檔讀改寫的鎖；與 launch_lock 分開，啟動中仍可瀏覽設定。
    pub config_lock: Mutex<()>,
    /// 取消進行中的下載（衛月；程式更新、遊戲更新也取同一個世代）；不經 launch_lock（下載進行時鎖正被佔用）。
    pub download_cancel: DownloadCancel,
    /// 各區服待套用的遊戲更新（版本檢查取得的清單；不含 UID／SID，只在記憶體）。
    pub pending_updates: PendingUpdates,
    /// ACT 連動指令檔的寫入者（登入寫入與遊戲結束後的切回共用一把鎖，protocols §6.2）。
    pub act_targets: act_bridge::ActTargets,
    /// 登入前先準備好的衛月（`install_dalamud` 的結果），緊接著的 `launch_account` 沿用一次。
    pub prepared_dalamud: dalamud::PreparedCache,
    /// FFXIV 插件最新版的記憶體快取（protocols §6.6）。
    pub act_plugin_latest: LatestCache,
    /// 本程式開的遊戲結束時判定角色「最後由哪個帳號使用」。
    pub character_usage: characters::usage::UsageWatch,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountView {
    #[serde(flatten)]
    pub account: Account,
    pub has_password: bool,
    pub has_otp_secret: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigView {
    pub games: GamePaths,
    pub accounts: Vec<AccountView>,
    pub profiles: Vec<Profile>,
    pub profiles_in_use: Vec<String>,
    pub profile_info: Vec<ProfileInfo>,
    /// 已下載的 Dalamud 本體；算 `profile_info` 時已讀過，一併回傳，前端重繪不必再列一次。
    pub installed: Vec<InstalledVersion>,
    pub accounts_running: Vec<String>,
    pub data_dir: String,
    pub ui: UiSettings,
}

#[tauri::command]
pub async fn get_config(state: State<'_, AppState>) -> AppResult<ConfigView> {
    let _guard = state.config_lock.lock().await;
    let config = store::load_config(&state.paths)?;
    let mut accounts = Vec::with_capacity(config.accounts.len());
    for account in config.accounts {
        accounts.push(AccountView {
            has_password: secrets::exists(&account.id, SecretKind::Password)?,
            has_otp_secret: secrets::exists(&account.id, SecretKind::OtpSecret)?,
            account,
        });
    }
    let mut profiles_in_use = Vec::new();
    for profile in &config.profiles {
        if dalamud::profile_in_use(&state.paths, &profile.id)? {
            profiles_in_use.push(profile.id.clone());
        }
    }
    let installed = dalamud::list_installed(&state.paths)?;
    let profile_info = config
        .profiles
        .iter()
        .map(|p| dalamud::profile_info(&state.paths, p, &installed))
        .collect();
    Ok(ConfigView {
        games: config.games,
        accounts,
        profiles: config.profiles,
        profiles_in_use,
        profile_info,
        installed,
        accounts_running: dalamud::running_accounts(&state.paths)?,
        data_dir: state.paths.root().display().to_string(),
        ui: config.ui,
    })
}

pub(crate) async fn update_config(
    state: &AppState,
    change: impl FnOnce(&mut Config) -> AppResult<()>,
) -> AppResult<()> {
    let _guard = state.config_lock.lock().await;
    let mut config = store::load_config(&state.paths)?;
    change(&mut config)?;
    store::save_config(&state.paths, &config)
}

#[tauri::command]
pub async fn set_game_path(
    state: State<'_, AppState>,
    region: Region,
    path: Option<String>,
) -> AppResult<()> {
    let path = path.map(|p| PathBuf::from(p.trim())).filter(|p| !p.as_os_str().is_empty());
    if let Some(p) = &path
        && !crate::game::is_valid_install(p)
    {
        return Err(AppError::GameFileMissing {
            path: crate::game::game_exe(p).display().to_string(),
        });
    }
    update_config(&state, |c| {
        c.games.set(region, path);
        Ok(())
    })
    .await
}

/// 設定 › 介面 › 主題：儲存後套到所有視窗（標題列與 WebView2 的 `prefers-color-scheme`）。
#[tauri::command]
pub async fn set_theme(app: AppHandle, state: State<'_, AppState>, theme: Theme) -> AppResult<()> {
    update_config(&state, |c| {
        c.ui.theme = theme;
        Ok(())
    })
    .await?;
    app.set_theme(main_window::window_theme(theme));
    Ok(())
}

/// 設定 › 介面 › 遊戲啟動後：下次成功啟動遊戲時生效。
#[tauri::command]
pub async fn set_after_launch(state: State<'_, AppState>, after_launch: AfterLaunch) -> AppResult<()> {
    update_config(&state, |c| {
        c.ui.after_launch = after_launch;
        Ok(())
    })
    .await
}

// 同步 command 在 main thread 執行、期間視窗不回應；讀 registry／磁碟的查詢（這兩個與 list_installed_dalamud、get_patch_state）宣告為 async。
#[tauri::command]
pub async fn detect_game_path(region: Region) -> Option<String> {
    crate::game::detect_install(region).map(|p| p.display().to_string())
}

/// 偵測舊啟動器留下、可直接沿用的衛月資料夾。
#[tauri::command]
pub async fn detect_dalamud_data() -> Vec<DetectedDalamudData> {
    dalamud::detect_from_env()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatusView {
    pub global: ServerStatus,
    pub tw: ServerStatus,
}

/// 兩服伺服器狀態，同時查詢；取得失敗或格式不符為 `unknown`。國際服是登入閘道狀態，
/// 台服是官網 World 狀態頁（只用於顯示，不擋登入）。
#[tauri::command]
pub async fn get_server_status() -> ServerStatusView {
    let (global, tw) = tokio::join!(global::fetch_gate_status(), crate::tw::fetch_world_status());
    ServerStatusView { global, tw }
}

/// 前端只指定要開哪一種資料夾，實際路徑一律由後端依設定或偵測結果解析。
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum FolderTarget {
    DataRoot,
    Profile { id: String },
    ProfileLogs { id: String },
    /// 偵測到的舊衛月資料夾；路徑必須在 `detected` 之中，不接受任意路徑。
    Detected { path: PathBuf },
    /// 該區服的遊戲設定資料夾（`文件\My Games\` 下）；不存在時不建立。
    GameConfig { region: Region },
    /// 角色的備份資料夾（protocols §7、`characters`）；不存在時不建立。
    CharacterBackups { region: Region, id: String },
}

fn folder_path(
    paths: &AppPaths,
    config: &Config,
    target: &FolderTarget,
    detected: &[DetectedDalamudData],
) -> AppResult<PathBuf> {
    let dir = match target {
        FolderTarget::DataRoot => paths.root().to_path_buf(),
        FolderTarget::Profile { id } => {
            dalamud::profile_data_dir(paths, launch::find_profile(config, id)?)?
        }
        FolderTarget::ProfileLogs { id } => {
            dalamud::profile_log_dir(paths, launch::find_profile(config, id)?)?
        }
        FolderTarget::Detected { path } => find_detected(detected, path)?.path.clone(),
        FolderTarget::CharacterBackups { region, id } => {
            characters::check_id(id)?;
            characters::backups_of(&paths.character_backups_dir(*region), id)
        }
        FolderTarget::GameConfig { region } => {
            let dir = crate::game::config_dir(*region)?;
            if !dir.is_dir() {
                return Err(AppError::NotFound {
                    what: format!("{region}遊戲設定資料夾（{}），請先啟動遊戲一次", dir.display()),
                });
            }
            return Ok(dir);
        }
    };
    if !dir.is_dir() {
        return Err(AppError::NotFound {
            what: format!("資料夾 {}", dir.display()),
        });
    }
    Ok(dir)
}

/// 以檔案總管開啟資料夾；路徑以 argv 傳遞，不經 shell。
#[tauri::command]
pub fn open_folder(state: State<'_, AppState>, target: FolderTarget) -> AppResult<()> {
    let config = store::load_config(&state.paths)?;
    let detected = match target {
        FolderTarget::Detected { .. } => dalamud::detect_from_env(),
        _ => Vec::new(),
    };
    let dir = folder_path(&state.paths, &config, &target, &detected)?;
    // explorer.exe 的 exit code 不代表成敗（成功開啟也常回 1），只確認能啟動。
    std::process::Command::new("explorer.exe")
        .arg(&dir)
        .spawn()
        .map(drop)
        .map_err(|e| AppError::io("開啟檔案總管", e))
}

/// 繁中服官網最新 5 則消息（protocols §2.7），依日期由新到舊。取不到（網路、HTTP、逾時、找不到清單區塊）時回錯誤；
/// 沒有有效項目時回空陣列。只用於登入頁側欄顯示。
#[tauri::command]
pub async fn get_tw_news() -> AppResult<Vec<crate::news::NewsItem>> {
    crate::tw::fetch_news().await
}

/// 國際服官方公告最新 5 則（protocols §1.4.2），`language` 是帳號的客戶端語言，決定公告語言與 Lodestone 主機。
/// 取不到（網路、HTTP、逾時、格式不符）時回錯誤；沒有有效項目時回空陣列。只用於登入頁側欄顯示。
#[tauri::command]
pub async fn get_global_news(language: crate::region::GlobalLanguage) -> AppResult<Vec<crate::news::NewsItem>> {
    global::fetch_news(language).await
}

/// 以系統預設瀏覽器開啟 `region` 的消息。不信任前端傳來的網址：開啟前再檢查一次該服白名單（protocols §1.4.2、§2.7），
/// 不符就回錯誤、不開啟。不加 async：同步 command 在主執行緒（WebView2 所在的 STA 執行緒）執行，
/// `ShellExecuteExW` 可能經 COM 委派給 shell extension，官方建議先初始化 COM。
#[tauri::command]
pub fn open_news(region: Region, url: String) -> AppResult<()> {
    crate::news::open_news(region, &url)
}

/// 不 derive `Debug`：`password`／`otp_secret` 是 secret，不得出現在任何 Debug／log 輸出。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountInput {
    /// 空字串＝新增。
    pub id: String,
    pub name: String,
    pub region: Region,
    pub username: String,
    pub otp: store::OtpMode,
    pub profile_id: Option<String>,
    #[serde(default)]
    pub language: crate::region::GlobalLanguage,
    /// `false`＝每次啟動手動輸入：刪除已存密碼，且不儲存本次帶來的 `password`。
    #[serde(default = "store::default_true")]
    pub remember_password: bool,
    /// 有值才更新密碼（僅 `remember_password` 為 true 時）。
    pub password: Option<String>,
    /// 有值才更新 OTP 金鑰（base32）。
    pub otp_secret: Option<String>,
    /// `true` 且沒帶新的 `otp_secret`＝刪除已存的 OTP 金鑰。
    #[serde(default)]
    pub clear_otp_secret: bool,
    #[serde(default)]
    pub extra_args: String,
    /// 「ACT 跟這個帳號」（`Account::act_follow`）。
    #[serde(default = "store::default_true")]
    pub act_follow: bool,
}

/// `save_account` 對 Credential Manager 中密碼的處置。
enum PasswordAction<'a> {
    Store(&'a str),
    Delete,
    Keep,
}

/// 不記住密碼一律刪除（即使帶了密碼）；記住且帶非空密碼才寫入；否則不動已存密碼。
fn password_action(remember: bool, typed: Option<&str>) -> PasswordAction<'_> {
    if !remember {
        return PasswordAction::Delete;
    }
    match typed.filter(|p| !p.is_empty()) {
        Some(password) => PasswordAction::Store(password),
        None => PasswordAction::Keep,
    }
}

#[tauri::command]
pub async fn save_account(state: State<'_, AppState>, input: AccountInput) -> AppResult<String> {
    let id = if input.id.is_empty() {
        uuid::Uuid::new_v4().simple().to_string()
    } else {
        input.id.clone()
    };
    if let Some(secret) = input.otp_secret.as_deref().filter(|s| !s.trim().is_empty()) {
        totp::parse_secret(secret)?;
    }
    let extra_args = crate::game::parse_extra_args(&input.extra_args)?
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ");
    let account = Account {
        id: id.clone(),
        name: input.name,
        region: input.region,
        username: input.username,
        otp: input.otp,
        profile_id: input.profile_id.filter(|p| !p.is_empty()),
        language: input.language,
        remember_password: input.remember_password,
        extra_args,
        act_follow: input.act_follow,
    };
    let password = password_action(input.remember_password, input.password.as_deref());
    let otp_secret = input.otp_secret.as_deref().map(str::trim).filter(|s| !s.is_empty());
    // 刪除 secret 在寫設定之前：刪不掉就回錯、設定不變，使用者可以重試。反過來時設定已寫成「不記住」，刪失敗的
    // 密碼卻留在 Credential Manager。刪成功但寫設定失敗時，帳號維持舊設定、只少了該 secret；畫面依 Credential
    // Manager 顯示有沒有存（`has_password`／`has_otp_secret`），啟動時會要求輸入，不比原本更糟。寫入 secret 仍在
    // 寫設定之後：新帳號的設定寫不成時，不留下沒有帳號的 secret。
    if matches!(password, PasswordAction::Delete) {
        secrets::delete(&id, SecretKind::Password)?;
    }
    if otp_secret.is_none() && input.clear_otp_secret {
        secrets::delete(&id, SecretKind::OtpSecret)?;
    }
    update_config(&state, |c| {
        if let Some(pid) = &account.profile_id {
            check_profile_region(c, pid, account.region)?;
        }
        match c.accounts.iter_mut().find(|a| a.id == account.id) {
            Some(existing) => *existing = account,
            None => c.accounts.push(account),
        }
        Ok(())
    })
    .await?;
    if let PasswordAction::Store(typed) = password {
        secrets::store(&id, SecretKind::Password, typed)?;
    }
    if let Some(secret) = otp_secret {
        secrets::store(&id, SecretKind::OtpSecret, secret)?;
    }
    Ok(id)
}

/// 不 derive `Debug`：code 是當下有效的 OTP，不得出現在 log。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OtpPreview {
    pub code: String,
    pub seconds_left: u64,
}

/// 畫面上預覽目前的 OTP，供使用者與驗證器 App 比對金鑰。
/// `secret` 非空白就用它（不儲存）；否則用 `account_id` 已存的 OTP 金鑰。
#[tauri::command]
pub async fn preview_otp(account_id: Option<String>, secret: Option<String>) -> AppResult<OtpPreview> {
    let secret = match secret.filter(|s| !s.trim().is_empty()) {
        Some(typed) => typed,
        None => account_id
            .as_deref()
            .map(|id| secrets::load(id, SecretKind::OtpSecret))
            .transpose()?
            .flatten()
            .ok_or_else(|| AppError::NotFound {
                what: "OTP 金鑰".into(),
            })?,
    };
    let (code, seconds_left) = totp::current_code_with_seconds_left(&totp::parse_secret(&secret)?);
    Ok(OtpPreview { code, seconds_left })
}

/// 綁定的設定檔必須存在且與帳號同區服。
fn check_profile_region(config: &Config, profile_id: &str, region: Region) -> AppResult<()> {
    if launch::find_profile(config, profile_id)?.region() != region {
        return Err(AppError::ProfileRegionMismatch);
    }
    Ok(())
}

/// 帳號要套用的衛月設定：解除、既有設定檔，或偵測到的舊資料夾。
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum BindTarget {
    None,
    Profile { id: String },
    Detected { path: PathBuf },
}

/// 沿用偵測資料時的新設定檔；欄位與前端 `profileFromDetected` 一致。
fn profile_from_detected(id: String, d: &DetectedDalamudData) -> Profile {
    let channel = match d.region {
        Region::Global => store::DalamudChannel::Goatcorp,
        Region::Tw => store::DalamudChannel::Yanmucorp {
            version: store::YanmucorpVersion::Latest,
        },
    };
    Profile {
        id,
        name: d.label.clone(),
        channel,
        injection_delay_ms: d.region.default_injection_delay_ms(),
        data_dir: Some(d.path.clone()),
        share_confirmed: false,
    }
}

/// 偵測結果中路徑等於 `path` 的那一項；不在偵測結果中回 NotFound（前端不能指定任意路徑）。
fn find_detected<'a>(
    detected: &'a [DetectedDalamudData],
    path: &Path,
) -> AppResult<&'a DetectedDalamudData> {
    detected
        .iter()
        .find(|d| d.path == path)
        .ok_or_else(|| AppError::NotFound {
            what: "Dalamud 資料夾".into(),
        })
}

/// 已有設定檔沿用 `found` 的資料夾就回它，否則依偵測結果新增一個；回傳在 `config.profiles` 的位置。
fn find_or_create_detected_profile(config: &mut Config, found: &DetectedDalamudData) -> usize {
    if let Some(index) = config
        .profiles
        .iter()
        .position(|p| p.data_dir.as_ref() == Some(&found.path))
    {
        return index;
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    config.profiles.push(profile_from_detected(id, found));
    config.profiles.len() - 1
}

/// 把偵測到的資料夾變成設定檔（不綁任何帳號），回傳 profile id；已匯入過就回原本那個。
/// `name` 非空時作為設定檔名稱（新建或改名）。
fn import_detected(
    config: &mut Config,
    path: &Path,
    name: Option<&str>,
    detected: &[DetectedDalamudData],
) -> AppResult<String> {
    let found = find_detected(detected, path)?;
    let index = find_or_create_detected_profile(config, found);
    let profile = &mut config.profiles[index];
    if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
        profile.name = name.to_string();
    }
    Ok(profile.id.clone())
}

#[tauri::command]
pub async fn import_detected_profile(
    state: State<'_, AppState>,
    path: PathBuf,
    name: Option<String>,
) -> AppResult<String> {
    let detected = dalamud::detect_from_env();
    let mut id = String::new();
    update_config(&state, |c| {
        id = import_detected(c, &path, name.as_deref(), &detected)?;
        Ok(())
    })
    .await?;
    Ok(id)
}

/// 只改帳號的 `profile_id`（`Detected` 必要時另新增一個設定檔），回傳綁定後的 profile id。
/// `Detected` 的路徑必須在 `detected` 之中且與帳號同區服；已有設定檔沿用該資料夾就直接綁它。
fn bind_account_profile(
    config: &mut Config,
    account_id: &str,
    target: BindTarget,
    detected: &[DetectedDalamudData],
) -> AppResult<Option<String>> {
    let region = config
        .accounts
        .iter()
        .find(|a| a.id == account_id)
        .ok_or_else(|| AppError::NotFound { what: "帳號".into() })?
        .region;
    let profile_id = match target {
        BindTarget::None => None,
        BindTarget::Profile { id } => {
            check_profile_region(config, &id, region)?;
            Some(id)
        }
        BindTarget::Detected { path } => {
            let found = find_detected(detected, &path)?;
            if found.region != region {
                return Err(AppError::ProfileRegionMismatch);
            }
            let index = find_or_create_detected_profile(config, found);
            let id = config.profiles[index].id.clone();
            check_profile_region(config, &id, region)?;
            Some(id)
        }
    };
    if let Some(account) = config.accounts.iter_mut().find(|a| a.id == account_id) {
        account.profile_id = profile_id.clone();
    }
    Ok(profile_id)
}

#[tauri::command]
pub async fn set_account_profile(
    state: State<'_, AppState>,
    account_id: String,
    target: BindTarget,
) -> AppResult<Option<String>> {
    let detected = match target {
        BindTarget::Detected { .. } => dalamud::detect_from_env(),
        _ => Vec::new(),
    };
    let mut bound = None;
    update_config(&state, |c| {
        bound = bind_account_profile(c, &account_id, target, &detected)?;
        Ok(())
    })
    .await?;
    Ok(bound)
}

#[tauri::command]
pub async fn delete_account(state: State<'_, AppState>, id: String) -> AppResult<()> {
    // 先刪 secret 再移除帳號：刪不掉就回錯、帳號保留，使用者可以重試；反過來時帳號已從畫面消失，刪失敗的密碼與
    // OTP 金鑰卻留在 Credential Manager。刪成功但寫設定失敗時，帳號仍在、只是沒有已存的 secret（畫面照實顯示），
    // 重按刪除即可，不比原本更糟。
    secrets::delete(&id, SecretKind::Password)?;
    secrets::delete(&id, SecretKind::OtpSecret)?;
    update_config(&state, |c| {
        c.accounts.retain(|a| a.id != id);
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn save_profile(state: State<'_, AppState>, mut profile: Profile) -> AppResult<String> {
    if profile.id.is_empty() {
        profile.id = uuid::Uuid::new_v4().simple().to_string();
    }
    let id = profile.id.clone();
    let detected = match profile.data_dir {
        Some(_) => dalamud::detect_from_env(),
        None => Vec::new(),
    };
    update_config(&state, |c| {
        let region = profile.region();
        if c.accounts
            .iter()
            .any(|a| a.profile_id.as_deref() == Some(id.as_str()) && a.region != region)
        {
            return Err(AppError::ProfileRegionMismatch);
        }
        check_data_dir(&c.profiles, &profile, &detected)?;
        match c.profiles.iter_mut().find(|p| p.id == profile.id) {
            Some(existing) => *existing = profile,
            None => c.profiles.push(profile),
        }
        Ok(())
    })
    .await?;
    Ok(id)
}

/// 沿用的資料夾必須是偵測到、同區服、且沒有被其他 profile 沿用的那一個。
fn check_data_dir(
    profiles: &[Profile],
    profile: &Profile,
    detected: &[DetectedDalamudData],
) -> AppResult<()> {
    let Some(dir) = &profile.data_dir else {
        return Ok(());
    };
    let found = find_detected(detected, dir)?;
    if found.region != profile.region() {
        return Err(AppError::ProfileRegionMismatch);
    }
    if let Some(other) = profiles
        .iter()
        .find(|p| p.id != profile.id && p.data_dir.as_ref() == Some(dir))
    {
        return Err(AppError::DataDirTaken {
            profile: other.name.clone(),
        });
    }
    Ok(())
}

/// 只移除設定，profile 資料夾（插件與設定）保留，避免誤刪使用者資料。
#[tauri::command]
pub async fn delete_profile(state: State<'_, AppState>, id: String) -> AppResult<()> {
    update_config(&state, |c| {
        if c.accounts.iter().any(|a| a.profile_id.as_deref() == Some(id.as_str())) {
            return Err(AppError::ProfileHasAccounts);
        }
        c.profiles.retain(|p| p.id != id);
        Ok(())
    })
    .await
}

/// `duplicate_profile` 的結果。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicatedProfile {
    pub id: String,
    pub name: String,
    /// 略過、沒有複製的 symlink／junction 數。
    pub skipped_links: u64,
}

/// 不與任何設定檔同名：`base` 已被用就依序接「 2」「 3」…。
fn unique_profile_name(profiles: &[Profile], base: &str) -> String {
    let taken = |name: &str| profiles.iter().any(|p| p.name == name);
    if !taken(base) {
        return base.to_string();
    }
    (2u32..)
        .map(|n| format!("{base} {n}"))
        .find(|name| !taken(name))
        .unwrap_or_else(|| base.to_string())
}

/// 新增 `source_id` 的副本設定：沿用區服、通道與注入延遲；資料在本程式的 `profiles\<new_id>\`（`data_dir`
/// 為 `None`）；帳號綁定不複製。`name` 空白時用「<原名> 複本」，重名加編號。回傳新設定檔名稱。
fn add_duplicate(
    config: &mut Config,
    source_id: &str,
    new_id: String,
    name: Option<&str>,
) -> AppResult<String> {
    let source = launch::find_profile(config, source_id)?;
    let base = name
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map_or_else(|| format!("{} 複本", source.name), str::to_string);
    let profile = Profile {
        id: new_id,
        name: unique_profile_name(&config.profiles, &base),
        data_dir: None,
        // 複本是新的一份資料，不沿用來源的共用確認（protocols §3）。
        share_confirmed: false,
        ..source.clone()
    };
    let created = profile.name.clone();
    config.profiles.push(profile);
    Ok(created)
}

/// 複製設定檔：設定與插件本體（不含插件資料與快取、`dalamudVfs.db`、log，見 `dalamud::profile`）一律複製進
/// 本程式的 `profiles\<新 id>\`，設定沿用、帳號綁定不複製。
/// 持 app-wide 鎖：複製期間不會啟動遊戲改寫來源，`staging\` 也只在鎖內動；來源正被遊戲使用就拒絕。
/// 資料完整複製後才寫設定，任何一步失敗都不留資料夾也不寫設定。可用 `cancel_dalamud_download` 取消。
#[tauri::command]
pub async fn duplicate_profile(
    state: State<'_, AppState>,
    id: String,
    name: Option<String>,
    on_progress: Channel<Progress>,
) -> AppResult<DuplicatedProfile> {
    let report = move |p: Progress| {
        // quality-allow: 進度只影響畫面，送不出去不影響複製本身
        let _ = on_progress.send(p);
    };
    duplicate(&state, &id, name.as_deref(), &report).await
}

async fn duplicate(
    state: &AppState,
    id: &str,
    name: Option<&str>,
    progress: &ProgressFn,
) -> AppResult<DuplicatedProfile> {
    let _guard = state.launch_lock.lock().await;
    let cancel = state.download_cancel.token();
    let source = {
        let _config = state.config_lock.lock().await;
        launch::find_profile(&store::load_config(&state.paths)?, id)?.clone()
    };
    if dalamud::profile_in_use(&state.paths, &source.id)? {
        return Err(AppError::DuplicateProfileRunning {
            profile: source.name,
        });
    }
    let new_id = uuid::Uuid::new_v4().simple().to_string();
    let copied = dalamud::duplicate_profile_data(&state.paths, &source, &new_id, progress, &cancel).await?;
    let mut created = String::new();
    let saved = update_config(state, |c| {
        created = add_duplicate(c, id, new_id.clone(), name)?;
        Ok(())
    })
    .await;
    if let Err(e) = saved {
        dalamud::discard_profile_dir(&state.paths, &new_id);
        return Err(e);
    }
    Ok(DuplicatedProfile {
        id: new_id,
        name: created,
        skipped_links: copied.skipped_links,
    })
}

#[tauri::command]
pub async fn list_yanmucorp_releases() -> AppResult<Vec<ReleaseInfo>> {
    dalamud::list_releases().await
}

#[tauri::command]
pub async fn list_installed_dalamud(state: State<'_, AppState>) -> AppResult<Vec<InstalledVersion>> {
    dalamud::list_installed(&state.paths)
}

#[tauri::command]
pub async fn remove_installed_dalamud(
    state: State<'_, AppState>,
    source: String,
    version: String,
) -> AppResult<()> {
    let _guard = state.launch_lock.lock().await;
    dalamud::remove_installed(&state.paths, &source, &version)
}

/// 區服對應的衛月來源：國際服只有 goatcorp 最新版（忽略 `version`），台服必須指定版本。
fn install_channel(region: Region, version: Option<YanmucorpVersion>) -> AppResult<DalamudChannel> {
    match region {
        Region::Global => Ok(DalamudChannel::Goatcorp),
        Region::Tw => version
            .map(|version| DalamudChannel::Yanmucorp { version })
            .ok_or_else(|| AppError::Install {
                context: format!("未指定{} Dalamud 版本", Region::Tw),
            }),
    }
}

/// 下載並安裝某區服的衛月（不啟動遊戲）；安裝目錄依版本共用，與設定檔無關。回傳安裝好的版本。
/// `version_check` 只在登入前準備、使用者本次選了「強制載入」時為 `Skip`（protocols §1.5）。
#[tauri::command]
pub async fn install_dalamud(
    app: AppHandle,
    state: State<'_, AppState>,
    region: Region,
    version: Option<YanmucorpVersion>,
    version_check: dalamud::GameVersionCheck,
) -> AppResult<String> {
    let channel = install_channel(region, version)?;
    let _guard = state.launch_lock.lock().await;
    let cancel = state.download_cancel.token();
    let config = store::load_config(&state.paths)?;
    let install = config
        .games
        .get(region)
        .ok_or(AppError::GameNotConfigured { region })?
        .to_path_buf();
    let prepared = launch::prepare_dalamud(
        &app,
        &state.paths,
        launch::install_progress_id(region),
        &channel,
        &install,
        version_check,
        &cancel,
    )
    .await?;
    state.prepared_dalamud.store(&channel, &install, &prepared);
    Ok(prepared.version)
}

/// 取消進行中的下載（`install_dalamud`、`launch_account` 的準備階段、`install_app_update`、
/// `update_game` 的下載階段）、`check_game_integrity` 與 `duplicate_profile`；沒有進行中的工作時為 no-op。
#[tauri::command]
pub fn cancel_dalamud_download(state: State<'_, AppState>) {
    state.download_cancel.cancel();
}

/// 前端 `check()` 要帶的 updater target：免安裝版 `windows-x86_64-portable`，安裝版 null（plugin 預設 `windows-x86_64`）。
#[tauri::command]
pub fn app_update_target(app: AppHandle) -> Option<&'static str> {
    update::flavor(&app).target()
}

/// 下載、驗簽並安裝程式更新（安裝版交給 NSIS、免安裝版原地替換 exe，見 `update.rs`）。安裝版成功時不會回傳
/// （updater 結束本程式，NSIS 裝完再開）；免安裝版在新版開始執行後回傳，本程式隨即結束。
/// 與衛月安裝、啟動共用 app-wide 鎖：本程式會在更新後結束，不能發生在安裝或登入途中。
#[tauri::command]
pub async fn install_app_update(
    app: AppHandle,
    state: State<'_, AppState>,
    on_progress: Channel<UpdateProgress>,
) -> AppResult<()> {
    let _guard = state.launch_lock.lock().await;
    let cancel = state.download_cancel.token();
    update::download_and_install(&app, &state.paths, on_progress, &cancel).await
}

/// 「設定 › 免責與安全」可閱讀的授權文件。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LicenseDoc {
    License,
    Notice,
    ThirdPartyNotices,
}

/// 授權文件全文。編進 exe：免安裝版沒有安裝目錄可放；THIRD-PARTY-NOTICES 約 570 KB，前端打開時才取。
#[tauri::command]
pub fn get_license_text(doc: LicenseDoc) -> &'static str {
    match doc {
        LicenseDoc::License => include_str!("../../LICENSE"),
        LicenseDoc::Notice => include_str!("../../NOTICE"),
        LicenseDoc::ThirdPartyNotices => include_str!("../../THIRD-PARTY-NOTICES.txt"),
    }
}

/// 成功（遊戲程序已開出）後依「介面 › 遊戲啟動後」收起主視窗；登入失敗、需要更新、取消時不動視窗。回傳後背景仍在
/// 觀察遊戲是否提早結束：提早結束時撤銷執行狀態、送 `launch::GAME_EXITED_EARLY_EVENT` 並叫回主視窗。
#[tauri::command]
pub async fn launch_account(
    app: AppHandle,
    state: State<'_, AppState>,
    main_window: State<'_, MainWindow>,
    account_id: String,
    options: LaunchOptions,
) -> AppResult<LaunchOutcome> {
    let _guard = state.launch_lock.lock().await;
    let cancel = state.download_cancel.token();
    let launch::Launched { outcome, early_exit } = launch::launch_account(
        &app,
        &state.paths,
        &state.pending_updates,
        &state.act_targets,
        &state.prepared_dalamud,
        &state.character_usage,
        &account_id,
        options,
        &cancel,
    )
    .await?;
    main_window.after_game_started(&app);
    // 收起之後才接手觀察結果：提早結束時的叫回一定在收起之後，不會被收起蓋掉。
    let (app, paths, window) = (app.clone(), state.paths.clone(), main_window.inner().clone());
    tokio::spawn(async move {
        early_exit
            .finish(&paths, |account_id, e| {
                launch::emit_exited_early(&app, account_id, e);
                window.restore(&app);
            })
            .await;
    });
    Ok(outcome)
}

/// 檢查遊戲更新：台服直接送版本檢查（不需登入）並更新 pending；國際服的清單要登入才拿得到，只回目前的 pending。
#[tauri::command]
pub async fn check_game_update(
    state: State<'_, AppState>,
    region: Region,
) -> AppResult<GameUpdateStatus> {
    // 台服會改寫 pending，與啟動、更新串行；國際服只讀 pending，不必等進行中的啟動。
    let _guard = match region {
        Region::Tw => Some(state.launch_lock.lock().await),
        Region::Global => None,
    };
    game_update::check_game_update(&state.paths, &state.pending_updates, region).await
}

/// 下載、核對並套用該區服 pending 的遊戲更新。全程持 app-wide 鎖（國際服含 UAC 等待與 helper 執行）：
/// 程式自更新、啟動與另一區服的更新都要等。國際服完成後要再按一次啟動（重新登入、重新取得 OTP）。
#[tauri::command]
pub async fn update_game(
    state: State<'_, AppState>,
    region: Region,
    on_progress: Channel<GameUpdateProgress>,
) -> AppResult<()> {
    let _guard = state.launch_lock.lock().await;
    let cancel = state.download_cancel.token();
    let report = move |p: GameUpdateProgress| {
        // quality-allow: 進度只影響畫面，送不出去不影響更新本身
        let _ = on_progress.send(p);
    };
    game_update::update_game(&state.paths, &state.pending_updates, region, &report, &cancel).await
}

/// 使用者確認已用官方啟動器修復後，清除該區服安裝的 dirty 標記。與啟動、更新串行。
#[tauri::command]
pub async fn clear_patch_dirty(state: State<'_, AppState>, region: Region) -> AppResult<()> {
    let _guard = state.launch_lock.lock().await;
    game_update::clear_dirty(&state.paths, region)
}

/// 各區服的 dirty 標記與 pending 摘要。
#[tauri::command]
pub async fn get_patch_state(state: State<'_, AppState>) -> AppResult<PatchState> {
    game_update::patch_state(&state.paths, &state.pending_updates)
}

/// 國際服完整性檢查（唯讀，不修復）：以 goatcorp 對照逐檔核對設定中的國際服安裝。台服沒有對照來源，
/// 不提供。持 app-wide 鎖：核對期間不得有更新或啟動改動遊戲檔；可用 `cancel_dalamud_download` 取消。
#[tauri::command]
pub async fn check_game_integrity(
    state: State<'_, AppState>,
    on_progress: Channel<Progress>,
) -> AppResult<IntegrityOutcome> {
    let _guard = state.launch_lock.lock().await;
    let cancel = state.download_cancel.token();
    let install = store::load_config(&state.paths)?
        .games
        .get(Region::Global)
        .ok_or(AppError::GameNotConfigured {
            region: Region::Global,
        })?
        .to_path_buf();
    let client = dalamud::http_client()?;
    let report = move |p: Progress| {
        // quality-allow: 進度只影響畫面，送不出去不影響核對本身
        let _ = on_progress.send(p);
    };
    integrity::check_integrity(&client, &install, &report, &cancel).await
}

async fn load_act_settings(state: &AppState) -> AppResult<ActConfig> {
    let _guard = state.config_lock.lock().await;
    Ok(store::load_config(&state.paths)?.act)
}

#[tauri::command]
pub async fn get_act_settings(state: State<'_, AppState>) -> AppResult<ActConfig> {
    load_act_settings(&state).await
}

/// 只改 `region` 那一組。`path` 去掉頭尾空白與引號（檔案總管「複製路徑」會加引號）後依
/// [`act::apply_settings`] 驗證並儲存。兩組可指向同一個執行檔（單開）。
#[tauri::command]
pub async fn save_act_settings(
    state: State<'_, AppState>,
    region: Region,
    path: Option<String>,
    launch_with_game: bool,
) -> AppResult<()> {
    let path = path
        .map(|p| PathBuf::from(p.trim().trim_matches('"')))
        .filter(|p| !p.as_os_str().is_empty());
    update_config(&state, |c| act::apply_settings(c.act.get_mut(region), path, launch_with_game)).await
}

/// 兩服 ACT 下拉選單的選項（`act_bridge::choices`）：系統安裝的 ACT（解除安裝登錄與預設安裝位置）、本程式建立的
/// 雙開 ACT、設定裡的其他路徑。讀登錄與檔案，不列舉程序。
#[tauri::command]
pub async fn list_act_choices(state: State<'_, AppState>) -> AppResult<Vec<ActChoice>> {
    let config = load_act_settings(&state).await?;
    Ok(act_bridge::choices(&config, &act_bridge::Env::from_env()?, act::detect()))
}

/// 本程式啟動、仍在執行的一個遊戲（帳號 id 與遊戲 pid）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunningGame {
    pub account_id: String,
    pub pid: u32,
}

/// 各區服 ACT 的狀態；兩組指向同一個執行檔（單開）時兩區狀態相同。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActStatusByRegion {
    pub global: ActStatus,
    pub tw: ActStatus,
}

/// 頂端狀態列：各區服 ACT 的狀態與仍在執行的遊戲。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunningStatus {
    pub act: ActStatusByRegion,
    pub games: Vec<RunningGame>,
}

/// 前端定時輪詢的輕量查詢。不經 launch_lock：只讀，與登入、更新、下載互不影響。
#[tauri::command]
pub async fn get_running_status(state: State<'_, AppState>) -> AppResult<RunningStatus> {
    let config = load_act_settings(&state).await?;
    // 單開時只查一次（每次查詢都會列舉系統程序）。
    let shared = matches!(
        (&config.global.path, &config.tw.path),
        (Some(global), Some(tw)) if act::same_location(global, tw)
    );
    let global = act::status(&config.global)?;
    let tw = if shared { global } else { act::status(&config.tw)? };
    let games = dalamud::running_account_pids(&state.paths)?
        .into_iter()
        .map(|(account_id, pid)| RunningGame { account_id, pid })
        .collect();
    Ok(RunningStatus {
        act: ActStatusByRegion { global, tw },
        games,
    })
}

/// 「立即開啟 ACT」（`region` 那一組）：已在執行就不再開。不經 launch_lock：與登入、更新、下載互不影響。
#[tauri::command]
pub async fn launch_act(state: State<'_, AppState>, region: Region) -> AppResult<ActLaunch> {
    act::start(load_act_settings(&state).await?.get(region).path.clone()).await
}

/// ACT 連動狀態（protocols §6.4）：各 ACT 是否已登記外掛、本程式建立的雙開 ACT；只讀檔案，不列舉程序。
#[tauri::command]
pub async fn get_act_link_status(state: State<'_, AppState>) -> AppResult<LinkStatus> {
    let config = load_act_settings(&state).await?;
    act_bridge::status(&config, &act_bridge::Env::from_env()?)
}

/// 各區服 ACT 的 FFXIV 插件版本與 GitHub 最新版（protocols §6.6）。最新版走記憶體快取（距上次查詢未滿一小時就沿用），
/// `force`（「重新檢查」）時重查；查不到最新版只回 `latest: null`。只讀檔案，不列舉程序、不改任何檔案。
#[tauri::command]
pub async fn get_act_plugin_versions(state: State<'_, AppState>, force: bool) -> AppResult<PluginVersions> {
    let config = load_act_settings(&state).await?;
    let latest = state.act_plugin_latest.get(force, act_plugin::fetch_latest).await;
    Ok(act_plugin::versions(&config, &act_bridge::Env::from_env()?, latest))
}

/// 「啟用 ACT 連動」（protocols §6.4）：把外掛登記到每個已設定 ACT 的設定檔；有 ACT 在執行就拒絕。
/// 持設定鎖：與「建立另一服的 ACT」互斥（兩者都改 ACT 設定檔）。
#[tauri::command]
pub async fn enable_act_link(state: State<'_, AppState>) -> AppResult<Vec<ActChange>> {
    let _guard = state.config_lock.lock().await;
    let config = store::load_config(&state.paths)?;
    act_bridge::enable_link(&config.act, &act_bridge::Env::from_env()?)
}

/// 「建立另一服的 ACT」（protocols §6.5）：建立雙開 ACT、兩個 ACT 都登記外掛，成功後寫回 ACT 設定。
#[tauri::command]
pub async fn create_dual_act(state: State<'_, AppState>) -> AppResult<DualResult> {
    let env = act_bridge::Env::from_env()?;
    let _guard = state.config_lock.lock().await;
    let mut config = store::load_config(&state.paths)?;
    let result = act_bridge::create_dual(&mut config.act, &env)?;
    store::save_config(&state.paths, &config)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_channel_follows_region() {
        let pinned = || YanmucorpVersion::Pinned { tag: "26-01-01-01".into() };
        assert_eq!(install_channel(Region::Global, None).expect("global"), DalamudChannel::Goatcorp);
        assert_eq!(
            install_channel(Region::Global, Some(pinned())).expect("global ignores version"),
            DalamudChannel::Goatcorp
        );
        assert_eq!(
            install_channel(Region::Tw, Some(pinned())).expect("tw"),
            DalamudChannel::Yanmucorp { version: pinned() }
        );
        assert!(matches!(install_channel(Region::Tw, None), Err(AppError::Install { .. })));
        // 前端送的 JSON 形狀。
        let version: Option<YanmucorpVersion> =
            serde_json::from_value(serde_json::json!({"kind": "latest"})).expect("latest");
        assert_eq!(version, Some(YanmucorpVersion::Latest));
        let none: Option<YanmucorpVersion> =
            serde_json::from_value(serde_json::Value::Null).expect("null");
        assert_eq!(none, None);
    }

    fn profile(id: &str, channel: DalamudChannel, data_dir: Option<&str>) -> Profile {
        Profile {
            id: id.into(),
            name: format!("name-{id}"),
            channel,
            injection_delay_ms: 0,
            data_dir: data_dir.map(PathBuf::from),
            share_confirmed: false,
        }
    }

    fn account(id: &str, region: Region, profile_id: Option<&str>) -> Account {
        Account {
            id: id.into(),
            name: format!("name-{id}"),
            region,
            username: format!("user-{id}"),
            otp: store::OtpMode::Manual,
            profile_id: profile_id.map(Into::into),
            language: Default::default(),
            remember_password: true,
            extra_args: String::new(),
            act_follow: true,
        }
    }

    fn detected(label: &str, region: Region, path: &str) -> DetectedDalamudData {
        DetectedDalamudData {
            label: label.into(),
            region,
            path: PathBuf::from(path),
            plugin_count: 0,
        }
    }

    #[test]
    fn bind_target_json_shape() {
        let parse = |json: serde_json::Value| -> BindTarget {
            serde_json::from_value(json).expect("target")
        };
        assert!(matches!(parse(serde_json::json!({"kind": "none"})), BindTarget::None));
        assert!(matches!(
            parse(serde_json::json!({"kind": "profile", "id": "g"})),
            BindTarget::Profile { id } if id == "g"
        ));
        assert!(matches!(
            parse(serde_json::json!({"kind": "detected", "path": r"C:\a\TC"})),
            BindTarget::Detected { path } if path == std::path::Path::new(r"C:\a\TC")
        ));
    }

    #[test]
    fn bind_account_profile_checks_region_and_only_touches_profile_id() {
        let tw = DalamudChannel::Yanmucorp {
            version: YanmucorpVersion::Latest,
        };
        let mut config = Config {
            accounts: vec![
                account("a", Region::Global, None),
                account("b", Region::Tw, Some("t")),
            ],
            profiles: vec![
                profile("g", DalamudChannel::Goatcorp, None),
                profile("t", tw, None),
            ],
            ..Default::default()
        };
        let profile_to = |id: &str| BindTarget::Profile { id: id.into() };
        let before = serde_json::to_value(&config.accounts[0]).expect("json");

        let bound = bind_account_profile(&mut config, "a", profile_to("g"), &[]).expect("bind");
        assert_eq!(bound.as_deref(), Some("g"));
        let mut after = serde_json::to_value(&config.accounts[0]).expect("json");
        assert_eq!(after["profileId"], "g");
        after["profileId"] = serde_json::Value::Null;
        assert_eq!(after, before);

        assert!(matches!(
            bind_account_profile(&mut config, "a", profile_to("t"), &[]),
            Err(AppError::ProfileRegionMismatch)
        ));
        assert!(matches!(
            bind_account_profile(&mut config, "a", profile_to("nope"), &[]),
            Err(AppError::NotFound { .. })
        ));
        assert!(matches!(
            bind_account_profile(&mut config, "nope", profile_to("g"), &[]),
            Err(AppError::NotFound { .. })
        ));
        assert_eq!(config.accounts[0].profile_id.as_deref(), Some("g"));

        let bound = bind_account_profile(&mut config, "b", BindTarget::None, &[]).expect("unbind");
        assert_eq!(bound, None);
        assert_eq!(config.accounts[1].profile_id, None);
        assert!(matches!(
            bind_account_profile(&mut config, "nope", BindTarget::None, &[]),
            Err(AppError::NotFound { .. })
        ));
        assert_eq!(config.profiles.len(), 2);
    }

    #[test]
    fn bind_detected_creates_profile_once_and_reuses_it() {
        let found = [
            detected("XIVLauncher", Region::Global, r"C:\a\XIVLauncher"),
            detected("TC 啟動器（共用）", Region::Tw, r"C:\a\TC"),
        ];
        let mut config = Config {
            accounts: vec![
                account("g1", Region::Global, None),
                account("t1", Region::Tw, None),
                account("t2", Region::Tw, None),
            ],
            ..Default::default()
        };
        let to = |path: &str| BindTarget::Detected { path: PathBuf::from(path) };
        let before = serde_json::to_value(&config.accounts[1]).expect("json");

        // 首次：建立設定檔並綁定，帳號只改 profileId。
        let tc_id = bind_account_profile(&mut config, "t1", to(r"C:\a\TC"), &found)
            .expect("bind")
            .expect("id");
        assert_eq!(config.profiles.len(), 1);
        let created = &config.profiles[0];
        assert_eq!(created.id, tc_id);
        assert_eq!(created.name, "TC 啟動器（共用）");
        assert!(matches!(
            created.channel,
            DalamudChannel::Yanmucorp {
                version: YanmucorpVersion::Latest
            }
        ));
        assert_eq!(created.injection_delay_ms, 5000);
        assert_eq!(created.data_dir.as_deref(), Some(std::path::Path::new(r"C:\a\TC")));
        let mut after = serde_json::to_value(&config.accounts[1]).expect("json");
        assert_eq!(after["profileId"], tc_id.as_str());
        after["profileId"] = serde_json::Value::Null;
        assert_eq!(after, before);

        // 第二個帳號選同一份舊資料：重用，不重複建立。
        let again = bind_account_profile(&mut config, "t2", to(r"C:\a\TC"), &found).expect("bind");
        assert_eq!(again.as_deref(), Some(tc_id.as_str()));
        assert_eq!(config.profiles.len(), 1);
        assert_eq!(config.accounts[2].profile_id.as_deref(), Some(tc_id.as_str()));

        // 國際服預設 goatcorp。
        let xl_id = bind_account_profile(&mut config, "g1", to(r"C:\a\XIVLauncher"), &found)
            .expect("bind")
            .expect("id");
        assert_eq!(config.profiles.len(), 2);
        let xl = config.profiles.iter().find(|p| p.id == xl_id).expect("created");
        assert!(matches!(xl.channel, DalamudChannel::Goatcorp));
        assert_eq!(xl.name, "XIVLauncher");
        assert_eq!(xl.injection_delay_ms, 0);

        // 不在偵測結果、異區服、帳號不存在：拒絕且不動設定。
        assert!(matches!(
            bind_account_profile(&mut config, "g1", to(r"C:\nope"), &found),
            Err(AppError::NotFound { .. })
        ));
        assert!(matches!(
            bind_account_profile(&mut config, "g1", to(r"C:\a\TC"), &found),
            Err(AppError::ProfileRegionMismatch)
        ));
        assert!(matches!(
            bind_account_profile(&mut config, "t1", to(r"C:\a\TC"), &[]),
            Err(AppError::NotFound { .. })
        ));
        assert!(matches!(
            bind_account_profile(&mut config, "nope", to(r"C:\a\TC"), &found),
            Err(AppError::NotFound { .. })
        ));
        assert_eq!(config.profiles.len(), 2);
        assert_eq!(config.accounts[0].profile_id.as_deref(), Some(xl_id.as_str()));
    }

    #[test]
    fn default_injection_delay_by_region() {
        assert_eq!(Region::Global.default_injection_delay_ms(), 0);
        assert_eq!(Region::Tw.default_injection_delay_ms(), 5000);
    }

    #[test]
    fn data_dir_must_be_detected_same_region_and_unshared() {
        let tw = DalamudChannel::Yanmucorp {
            version: YanmucorpVersion::Latest,
        };
        let detected = [
            DetectedDalamudData {
                label: "XIVLauncher".into(),
                region: Region::Global,
                path: PathBuf::from(r"C:\a\XIVLauncher"),
                plugin_count: 0,
            },
            DetectedDalamudData {
                label: "TC 啟動器（共用）".into(),
                region: Region::Tw,
                path: PathBuf::from(r"C:\a\TC"),
                plugin_count: 0,
            },
        ];
        let existing = [profile("other", tw.clone(), Some(r"C:\a\TC"))];

        assert!(check_data_dir(&existing, &profile("p", tw.clone(), None), &[]).is_ok());
        assert!(
            check_data_dir(
                &existing,
                &profile("p", DalamudChannel::Goatcorp, Some(r"C:\a\XIVLauncher")),
                &detected
            )
            .is_ok()
        );
        assert!(matches!(
            check_data_dir(&existing, &profile("p", tw.clone(), Some(r"C:\a\XIVLauncher")), &detected),
            Err(AppError::ProfileRegionMismatch)
        ));
        assert!(matches!(
            check_data_dir(&existing, &profile("p", tw.clone(), Some(r"C:\nope")), &detected),
            Err(AppError::NotFound { .. })
        ));
        assert!(matches!(
            check_data_dir(&existing, &profile("p", tw.clone(), Some(r"C:\a\TC")), &detected),
            Err(AppError::DataDirTaken { profile }) if profile == "name-other"
        ));
        // 自己重新存檔不算重複。
        assert!(check_data_dir(&existing, &existing[0], &detected).is_ok());
    }

    #[test]
    fn folder_targets_resolve_from_config_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().join("app"));
        let external = dir.path().join("XIVLauncher");
        let mut config = Config::default();
        config.profiles.push(profile("managed", DalamudChannel::Goatcorp, None));
        let mut adopted = profile("adopted", DalamudChannel::Goatcorp, None);
        adopted.data_dir = Some(external.clone());
        config.profiles.push(adopted);
        config.profiles.push(profile("..", DalamudChannel::Goatcorp, None));

        let target = |json: serde_json::Value| -> FolderTarget {
            serde_json::from_value(json).expect("target")
        };
        let resolve = |json: serde_json::Value| folder_path(&paths, &config, &target(json), &[]);
        let not_found = |r: AppResult<PathBuf>| matches!(r, Err(AppError::NotFound { .. }));

        // 目錄還不存在：一律 NotFound。
        assert!(not_found(resolve(serde_json::json!({"kind": "dataRoot"}))));
        assert!(not_found(resolve(serde_json::json!({"kind": "profile", "id": "managed"}))));

        for d in [
            paths.profile_dir("managed").join("logs"),
            paths.profile_dir("adopted").join("logs"),
            external.join("installedPlugins"),
        ] {
            std::fs::create_dir_all(d).expect("mkdir");
        }

        assert_eq!(
            resolve(serde_json::json!({"kind": "dataRoot"})).expect("root"),
            paths.root()
        );
        assert_eq!(
            resolve(serde_json::json!({"kind": "profile", "id": "managed"})).expect("managed"),
            paths.profile_dir("managed")
        );
        assert_eq!(
            resolve(serde_json::json!({"kind": "profileLogs", "id": "managed"})).expect("logs"),
            paths.profile_dir("managed").join("logs")
        );
        // 沿用型：資料夾是外部的，log 仍在本程式的 profiles\<id>\logs。
        assert_eq!(
            resolve(serde_json::json!({"kind": "profile", "id": "adopted"})).expect("adopted"),
            external
        );
        assert_eq!(
            resolve(serde_json::json!({"kind": "profileLogs", "id": "adopted"})).expect("logs"),
            paths.profile_dir("adopted").join("logs")
        );
        // `profiles\..` 雖然存在（＝資料根目錄），不安全的 id 仍拒絕；不在設定裡的 id 也拒絕。
        assert!(not_found(resolve(serde_json::json!({"kind": "profile", "id": ".."}))));
        assert!(not_found(resolve(serde_json::json!({"kind": "profileLogs", "id": ".."}))));
        assert!(not_found(resolve(serde_json::json!({"kind": "profile", "id": "ghost"}))));
    }

    /// 前端 `{ kind: "gameConfig", region }` 只帶區服，路徑由後端組（`game::config_dir`）。
    #[test]
    fn game_config_target_takes_only_a_region() {
        for (json, expected) in [("global", Region::Global), ("tw", Region::Tw)] {
            let target: FolderTarget =
                serde_json::from_value(serde_json::json!({"kind": "gameConfig", "region": json}))
                    .expect("target");
            assert!(matches!(target, FolderTarget::GameConfig { region } if region == expected));
        }
        assert!(
            serde_json::from_value::<FolderTarget>(serde_json::json!({"kind": "gameConfig", "path": r"C:\x"}))
                .is_err()
        );
    }

    #[test]
    fn detected_folder_target_must_be_a_detection_result() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().join("app"));
        let legacy = dir.path().join("XIVLauncher");
        std::fs::create_dir_all(&legacy).expect("mkdir");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("mkdir");
        let found = [DetectedDalamudData {
            label: "XIVLauncher".into(),
            region: Region::Global,
            path: legacy.clone(),
            plugin_count: 0,
        }];
        let config = Config::default();
        let resolve = |path: &Path, detected: &[DetectedDalamudData]| {
            let target: FolderTarget =
                serde_json::from_value(serde_json::json!({"kind": "detected", "path": path}))
                    .expect("target");
            folder_path(&paths, &config, &target, detected)
        };

        assert_eq!(resolve(&legacy, &found).expect("detected"), legacy);
        // 存在但不在偵測結果中的資料夾、偵測結果為空：一律拒絕。
        assert!(matches!(resolve(&elsewhere, &found), Err(AppError::NotFound { .. })));
        assert!(matches!(resolve(&legacy, &[]), Err(AppError::NotFound { .. })));
    }

    #[test]
    fn import_detected_creates_once_reuses_and_renames() {
        let found = [
            detected("XIVLauncher", Region::Global, r"C:\a\XIVLauncher"),
            detected("TC 啟動器（共用）", Region::Tw, r"C:\a\TC"),
        ];
        let mut config = Config {
            accounts: vec![account("t1", Region::Tw, None)],
            ..Default::default()
        };
        let tc = Path::new(r"C:\a\TC");

        // 首次：建立設定檔，名稱沿用偵測標籤；不綁任何帳號。
        let id = import_detected(&mut config, tc, None, &found).expect("import");
        assert_eq!(config.profiles.len(), 1);
        assert_eq!(config.profiles[0].id, id);
        assert_eq!(config.profiles[0].name, "TC 啟動器（共用）");
        assert_eq!(config.profiles[0].data_dir.as_deref(), Some(tc));
        assert!(matches!(
            config.profiles[0].channel,
            DalamudChannel::Yanmucorp {
                version: YanmucorpVersion::Latest
            }
        ));
        assert_eq!(config.accounts[0].profile_id, None);

        // 再次：回同一個 id，不重複建立；name 非空就改名，空白不改。
        let again = import_detected(&mut config, tc, Some("  台服主力  "), &found).expect("rename");
        assert_eq!(again, id);
        assert_eq!(config.profiles.len(), 1);
        assert_eq!(config.profiles[0].name, "台服主力");
        let blank = import_detected(&mut config, tc, Some("   "), &found).expect("blank");
        assert_eq!(blank, id);
        assert_eq!(config.profiles[0].name, "台服主力");

        // 綁定同一份舊資料時沿用已匯入的設定檔（共用找或建）。
        let bound = bind_account_profile(
            &mut config,
            "t1",
            BindTarget::Detected { path: tc.to_path_buf() },
            &found,
        )
        .expect("bind");
        assert_eq!(bound.as_deref(), Some(id.as_str()));
        assert_eq!(config.profiles.len(), 1);

        // 首次匯入就給名稱：新設定檔直接用它。
        let xl = import_detected(&mut config, Path::new(r"C:\a\XIVLauncher"), Some("國際服"), &found)
            .expect("named import");
        let created = config.profiles.iter().find(|p| p.id == xl).expect("created");
        assert_eq!(created.name, "國際服");
        assert!(matches!(created.channel, DalamudChannel::Goatcorp));

        // 不在偵測結果中：拒絕且不動設定。
        assert!(matches!(
            import_detected(&mut config, Path::new(r"C:\nope"), Some("x"), &found),
            Err(AppError::NotFound { .. })
        ));
        assert!(matches!(
            import_detected(&mut config, tc, None, &[]),
            Err(AppError::NotFound { .. })
        ));
        assert_eq!(config.profiles.len(), 2);
    }

    fn test_state(root: &Path) -> AppState {
        let paths = AppPaths::new(root.to_path_buf());
        AppState {
            act_targets: act_bridge::ActTargets::new(paths.clone()),
            prepared_dalamud: dalamud::PreparedCache::default(),
            character_usage: characters::usage::UsageWatch::new(paths.clone()),
            paths,
            launch_lock: Mutex::new(()),
            config_lock: Mutex::new(()),
            download_cancel: DownloadCancel::default(),
            pending_updates: PendingUpdates::default(),
            act_plugin_latest: LatestCache::default(),
        }
    }

    fn put(root: &Path, rel: &str, content: &[u8]) {
        let file = root.join(rel);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(file, content).expect("write");
    }

    /// `root` 底下每個項目（`/` 分隔的相對路徑）→ 檔案內容；資料夾與 reparse point 為 `None`，不進入 reparse point。
    fn tree(root: &Path) -> std::collections::BTreeMap<String, Option<Vec<u8>>> {
        let mut out = std::collections::BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read_dir") {
                let path = entry.expect("entry").path();
                let rel = path.strip_prefix(root).expect("rel").to_string_lossy().replace('\\', "/");
                let meta = std::fs::symlink_metadata(&path).expect("metadata");
                if meta.is_file() {
                    out.insert(rel, Some(std::fs::read(&path).expect("read")));
                } else {
                    if meta.is_dir() {
                        stack.push(path);
                    }
                    out.insert(rel, None);
                }
            }
        }
        out
    }

    fn entries(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect()
    }

    fn ignore_progress(_: Progress) {}

    /// 本程式管理的 profile：設定與插件本體（`installedPlugins`、`devPlugins` 整個、`pluginConfigs` 最上層的
    /// 一般檔案）複製成內容相同的獨立副本；`pluginConfigs` 的子資料夾（插件資料與快取）、`dalamudVfs.db`、
    /// `logs` 與 junction 不複製；設定沿用、帳號綁定不複製；重名加編號；暫存不留東西。
    #[tokio::test(flavor = "multi_thread")]
    async fn duplicate_copies_settings_and_plugins_but_not_plugin_data_into_independent_copy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = test_state(&dir.path().join("app"));
        let paths = &state.paths;
        let tw = DalamudChannel::Yanmucorp {
            version: YanmucorpVersion::Pinned { tag: "26-10-03-01".into() },
        };
        let mut source = profile("src", tw, None);
        source.injection_delay_ms = 1500;
        source.share_confirmed = true;
        let config = Config {
            accounts: vec![account("a", Region::Tw, Some("src"))],
            profiles: vec![source.clone()],
            ..Default::default()
        };
        store::save_config(paths, &config).expect("save");

        let src = paths.profile_dir("src");
        // 跨過複製緩衝區大小，確認大檔分段複製完整。
        let big: Vec<u8> = (0..3 * 1024 * 1024 + 7).map(|i| (i % 251) as u8).collect();
        for (rel, content) in [
            ("dalamudConfig.json", b"{\"a\":1}".as_slice()),
            ("dalamudUI.ini", b"[ui]"),
            ("dalamudVfs.db", b"vfs"),
            ("installedPlugins/A/1.0/A.dll", &big),
            ("installedPlugins/A/1.0/A.json", b"{}"),
            ("devPlugins/D/D.dll", b"dev"),
            ("pluginConfigs/A.json", b"{}"),
            ("pluginConfigs/B.yaml", b"b: 1"),
            ("pluginConfigs/A/nested/data.bin", b"data"),
            ("pluginConfigs/ShaderProbe/cache.bin", b"cache"),
            ("logs/dalamud.log", b"log"),
        ] {
            put(&src, rel, content);
        }
        std::fs::create_dir_all(src.join("pluginConfigs/Empty")).expect("mkdir");
        let outside = dir.path().join("outside");
        put(&outside, "shared.bin", b"outside");
        junction::create(&outside, src.join("pluginConfigs/Linked")).expect("junction");

        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&events);
        let record = move |p: Progress| sink.lock().expect("lock").push((p.done, p.total));
        let first = duplicate(&state, "src", None, &record).await.expect("duplicate");
        assert_eq!(first.name, "name-src 複本");
        assert_eq!(first.skipped_links, 1);

        let saved = store::load_config(paths).expect("load");
        assert_eq!(saved.profiles.len(), 2);
        let copy = saved.profiles.iter().find(|p| p.id == first.id).expect("new profile");
        assert_eq!(copy.name, first.name);
        assert_eq!(copy.channel, source.channel);
        assert_eq!(copy.injection_delay_ms, 1500);
        assert_eq!(copy.data_dir, None);
        assert!(!copy.share_confirmed, "複本不沿用來源的共用確認");
        assert_eq!(saved.accounts[0].profile_id.as_deref(), Some("src"));

        let dest = paths.profile_dir(&first.id);
        let mut expected = tree(&src);
        expected.retain(|k, v| match k.strip_prefix("pluginConfigs/") {
            Some(rest) => !rest.contains('/') && v.is_some(),
            None => !k.starts_with("logs") && k != "dalamudVfs.db",
        });
        assert_eq!(tree(&dest), expected);
        assert_eq!(
            expected.keys().filter(|k| k.starts_with("pluginConfigs")).collect::<Vec<_>>(),
            ["pluginConfigs", "pluginConfigs/A.json", "pluginConfigs/B.yaml"]
        );
        assert!(expected.contains_key("installedPlugins/A/1.0/A.dll") && expected.contains_key("devPlugins/D/D.dll"));
        let total: u64 = expected.values().flatten().map(|c| c.len() as u64).sum();
        assert_eq!(events.lock().expect("lock").last(), Some(&(total, Some(total))));
        assert!(entries(&paths.staging_dir()).is_empty());

        // 獨立副本：改副本不影響原本，junction 指向的資料也沒動。
        std::fs::write(dest.join("pluginConfigs/A.json"), b"changed").expect("write");
        std::fs::remove_dir_all(dest.join("installedPlugins/A")).expect("remove");
        assert_eq!(std::fs::read(src.join("pluginConfigs/A.json")).expect("read"), b"{}");
        assert!(src.join("installedPlugins/A/1.0/A.dll").is_file());
        assert_eq!(std::fs::read(outside.join("shared.bin")).expect("read"), b"outside");

        // 名稱重複加編號：預設名與指定名都是。
        let second = duplicate(&state, "src", None, &ignore_progress).await.expect("second");
        assert_eq!(second.name, "name-src 複本 2");
        let named = duplicate(&state, "src", Some("  name-src  "), &ignore_progress).await.expect("named");
        assert_eq!(named.name, "name-src 2");
        assert_eq!(store::load_config(paths).expect("load").profiles.len(), 4);
    }

    /// 沿用外部資料夾的 profile：只把設定與插件本體複製進本程式的 `profiles\<新 id>\`；啟動器自己的檔案、
    /// log、`dalamudVfs.db`、插件資料不帶走，外部資料夾不動；外部資料夾已不在則拒絕。
    #[tokio::test(flavor = "multi_thread")]
    async fn duplicate_external_profile_copies_into_own_profiles_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = test_state(&dir.path().join("app"));
        let paths = &state.paths;
        let external = dir.path().join("XIVLauncher");
        for rel in [
            "dalamudConfig.json",
            "dalamudVfs.db",
            "installedPlugins/B/B.dll",
            "pluginConfigs/B.json",
            "pluginConfigs/B/data.sqlite",
            "addon/Hooks/x.dll",
            "runtime/r.dll",
            "accountsList.json",
            "launcherConfigV3.json",
            "dalamud.log",
            "output.log",
        ] {
            put(&external, rel, rel.as_bytes());
        }
        let mut adopted = profile("ext", DalamudChannel::Goatcorp, None);
        adopted.data_dir = Some(external.clone());
        let mut gone = profile("gone", DalamudChannel::Goatcorp, None);
        gone.data_dir = Some(dir.path().join("missing"));
        let config = Config {
            profiles: vec![adopted, gone],
            ..Default::default()
        };
        store::save_config(paths, &config).expect("save");
        let before = tree(&external);

        let result = duplicate(&state, "ext", None, &ignore_progress).await.expect("duplicate");
        assert_eq!(result.name, "name-ext 複本");
        let saved = store::load_config(paths).expect("load");
        let copy = saved.profiles.iter().find(|p| p.id == result.id).expect("new profile");
        assert_eq!(copy.data_dir, None);
        assert_eq!(dalamud::profile_data_dir(paths, copy).expect("dir"), paths.profile_dir(&result.id));
        let copied: Vec<String> = tree(&paths.profile_dir(&result.id)).into_keys().collect();
        assert_eq!(
            copied,
            [
                "dalamudConfig.json",
                "installedPlugins",
                "installedPlugins/B",
                "installedPlugins/B/B.dll",
                "pluginConfigs",
                "pluginConfigs/B.json",
            ]
        );
        assert_eq!(
            std::fs::read(paths.profile_dir(&result.id).join("installedPlugins/B/B.dll")).expect("read"),
            b"installedPlugins/B/B.dll"
        );
        assert_eq!(tree(&external), before);

        let config_before = std::fs::read(paths.config_file()).expect("config");
        assert!(matches!(
            duplicate(&state, "gone", None, &ignore_progress).await,
            Err(AppError::NotFound { .. })
        ));
        assert_eq!(std::fs::read(paths.config_file()).expect("config"), config_before);
    }

    /// 來源正被遊戲使用就拒絕；複製途中失敗（來源檔被鎖住）不留資料夾、不寫設定、暫存清乾淨。
    #[tokio::test(flavor = "multi_thread")]
    async fn duplicate_refuses_running_profile_and_leaves_nothing_on_failure() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let state = test_state(&dir.path().join("app"));
        let paths = &state.paths;
        let config = Config {
            profiles: vec![
                profile("src", DalamudChannel::Goatcorp, None),
                profile("busy", DalamudChannel::Goatcorp, None),
            ],
            ..Default::default()
        };
        store::save_config(paths, &config).expect("save");
        for id in ["src", "busy"] {
            put(&paths.profile_dir(id), "dalamudConfig.json", b"{}");
            put(&paths.profile_dir(id), "installedPlugins/A/A.dll", b"dll");
            put(&paths.profile_dir(id), "pluginConfigs/locked.json", b"locked");
        }
        let config_before = std::fs::read(paths.config_file()).expect("config");
        let mut profile_dirs = entries(&paths.root().join("profiles"));
        profile_dirs.sort();

        let me = dalamud::process_identity(std::process::id())
            .expect("identity")
            .expect("current process is alive");
        dalamud::mark_running(paths, "busy", &me).expect("mark");
        assert!(matches!(
            duplicate(&state, "busy", None, &ignore_progress).await,
            Err(AppError::DuplicateProfileRunning { profile }) if profile == "name-busy"
        ));

        // 前面的檔案已複製進暫存後才遇到鎖住的檔案。
        let _locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(paths.profile_dir("src").join("pluginConfigs/locked.json"))
            .expect("lock");
        assert!(matches!(
            duplicate(&state, "src", None, &ignore_progress).await,
            Err(AppError::Io { .. })
        ));

        assert_eq!(std::fs::read(paths.config_file()).expect("config"), config_before);
        let mut after = entries(&paths.root().join("profiles"));
        after.sort();
        assert_eq!(after, profile_dirs);
        assert!(entries(&paths.staging_dir()).is_empty());
    }

    #[test]
    fn password_action_follows_remember_flag() {
        assert!(matches!(password_action(true, Some("pw")), PasswordAction::Store("pw")));
        assert!(matches!(password_action(true, Some("")), PasswordAction::Keep));
        assert!(matches!(password_action(true, None), PasswordAction::Keep));
        // 不記住：即使帶了密碼也不存，並刪除舊的。
        assert!(matches!(password_action(false, Some("pw")), PasswordAction::Delete));
        assert!(matches!(password_action(false, None), PasswordAction::Delete));
    }

    #[test]
    fn account_input_remember_password_defaults_to_true() {
        let base = serde_json::json!({
            "id": "",
            "name": "n",
            "region": "tw",
            "username": "u",
            "otp": "manual",
            "profileId": null,
            "password": null,
            "otpSecret": null
        });
        let input: AccountInput = serde_json::from_value(base.clone()).expect("legacy input");
        assert!(input.remember_password);
        // 舊前端不帶 clearOtpSecret：不刪已存的 OTP 金鑰。
        assert!(!input.clear_otp_secret);
        let mut manual = base;
        manual["rememberPassword"] = serde_json::Value::Bool(false);
        let input: AccountInput = serde_json::from_value(manual).expect("manual input");
        assert!(!input.remember_password);
    }
}
