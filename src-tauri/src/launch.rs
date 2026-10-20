//! 啟動編排：dirty 標記 →（國際服）閘道狀態 → 遊戲版本檢查 → 準備衛月 → 登入 → 啟動／注入 → 記錄佔用
//! →（帳號勾了「ACT 跟這個帳號」時）寫 ACT 連動指令檔並等這個遊戲結束（結束時切回，見 `act_bridge::ActTargets`）
//! →（同上，且該區服的 ACT 設定開啟時）開該區服的 ACT。遊戲開出就做後續步驟並回傳成功；是否提早結束（可能超過
//! 客戶端實例上限）在背景觀察（[`EarlyExitWatch`]），觀察到時撤銷執行狀態並以 [`GAME_EXITED_EARLY_EVENT`] 通知前端。
//! 登入前前端已先準備好衛月（`install_dalamud`）時沿用那次的結果（`dalamud::PreparedCache`），同一次登入不準備兩次。
//!
//! 遊戲版本檢查先於衛月相容 gate（主 note「流程」）：goatcorp 會比對遊戲版本，綁 profile 而遊戲是舊版時
//! 若先準備衛月，會因版本不符退出、走不到更新。台服先送獨立的版本檢查，再準備衛月、登入（登入流程內伺服器
//! 要求的那次檢查照送）；國際服的清單要登入才拿得到，順序為 boot 檢查 → 登入（含 session 註冊）→ 準備衛月。
//! 取得 patch list 時解析並存成該區服的 pending，回 `UpdateRequired`、不啟動；更新完成後由使用者再按一次
//! 啟動（國際服即重新登入、重新取得 OTP）。國際服維修中在登入之前就擋下；狀態查不到時照常繼續，由登入結果決定。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::task::JoinHandle;

use crate::act_bridge::ActTargets;
use crate::dalamud::{
    self, CancelToken, GameProcess, GameVersionCheck, InjectRequest, PluginLoad, PreparedCache, PreparedDalamud, Progress,
};
use crate::error::{AppError, AppResult};
use crate::global;
use crate::patch::dirty;
use crate::patch::update::{self, PendingUpdates};
use crate::region::{Region, TW_DALAMUD_LANGUAGE};
use crate::secrets::{self, SecretKind};
use crate::store::{self, Account, ActSettings, AppPaths, Config, DalamudChannel, OtpMode, Profile};
use crate::totp;
use crate::tw;

/// Lease 取不到時 OAuth Referer 用的 frontier 範本（XL Debug fallback）。
const FALLBACK_FRONTIER_URL: &str = "https://launcher.finalfantasyxiv.com/v650/index.html?rc_lang={0}&time={1}";

pub const PROGRESS_EVENT: &str = "launch-progress";

/// 背景觀察到遊戲提早結束時送給前端的事件（`launch_account` 已回成功）；`message` 與同步回錯時的錯誤訊息相同。
pub const GAME_EXITED_EARLY_EVENT: &str = "game-exited-early";

/// 不 derive `Debug`：`password` 是使用者輸入的密碼，不得出現在任何 Debug／log 輸出。
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchOptions {
    /// 國際服手動 OTP 模式時由使用者輸入。繁中服手動 OTP 在登入確認對話框輸入，不用這個欄位。
    pub otp: Option<String>,
    /// 本次輸入的密碼；非空時優先使用，只用於本次登入、不寫入任何地方。
    #[serde(default)]
    pub password: Option<String>,
    /// 本次不載入衛月。
    #[serde(default)]
    pub without_dalamud: bool,
    /// 本次載入哪些 plugin；不載入衛月時不使用。
    #[serde(default)]
    pub plugins: PluginLoad,
    /// 本次是否比對衛月支援的遊戲版本（只有 goatcorp 有這項比對）；不載入衛月時不使用。
    #[serde(default)]
    pub version_check: GameVersionCheck,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchOutcome {
    pub pid: u32,
    pub dalamud_version: Option<String>,
    /// 遊戲已啟動，但「登入遊戲後自動開啟 ACT」沒能開起 ACT 的原因；沒開或已開起為 `None`。
    pub act_warning: Option<String>,
    /// 遊戲已啟動，但 ACT 連動指令檔（protocols §6.2）沒寫成的原因；寫成為 `None`。
    pub act_target_warning: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProgressPayload<'a> {
    account_id: &'a str,
    stage: &'a str,
    done: u64,
    total: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GameExitedEarlyPayload<'a> {
    account_id: &'a str,
    message: String,
}

/// 送 [`GAME_EXITED_EARLY_EVENT`]。
pub fn emit_exited_early(app: &AppHandle, account_id: &str, error: &AppError) {
    let payload = GameExitedEarlyPayload {
        account_id,
        message: error.to_string(),
    };
    // 送不出去只少一則畫面通知：紀錄已撤銷，header 的輪詢照樣會更新。背景沒有畫面可回報，只記 log。
    if let Err(e) = app.emit(GAME_EXITED_EARLY_EVENT, payload) {
        eprintln!("無法通知前端遊戲提早結束：{e}");
    }
}

fn emit_stage(app: &AppHandle, account_id: &str, stage: &str) {
    // 進度事件只影響畫面；送不出去不影響啟動結果。
    let _ = app.emit(
        PROGRESS_EVENT,
        ProgressPayload {
            account_id,
            stage,
            done: 0,
            total: None,
        },
    );
}

fn find_account<'a>(config: &'a Config, id: &str) -> AppResult<&'a Account> {
    config
        .accounts
        .iter()
        .find(|a| a.id == id)
        .ok_or_else(|| AppError::NotFound { what: "帳號".into() })
}

pub fn find_profile<'a>(config: &'a Config, id: &str) -> AppResult<&'a Profile> {
    config
        .profiles
        .iter()
        .find(|p| p.id == id)
        .ok_or_else(|| AppError::NotFound {
            what: "Dalamud 設定檔".into(),
        })
}

/// 本次輸入（非空）優先；否則才讀已存密碼（`load_stored` 只在需要時呼叫）。皆無回
/// [`AppError::PasswordRequired`]。空字串視為未輸入；不 trim，密碼可含空白。
fn resolve_password(
    typed: Option<&str>,
    load_stored: impl FnOnce() -> AppResult<Option<String>>,
) -> AppResult<String> {
    if let Some(typed) = typed.filter(|p| !p.is_empty()) {
        return Ok(typed.to_string());
    }
    load_stored()?
        .filter(|p| !p.is_empty())
        .ok_or(AppError::PasswordRequired)
}

/// 帳號的 OTP 從哪來。開始登入前就解析好（金鑰讀不到、國際服手動 OTP 沒填都在這裡回錯），code 等到要用時才以
/// [`OtpSource::code`] 產生。不 derive `Debug`：含手動 OTP 與 TOTP 金鑰。
enum OtpSource {
    None,
    /// 國際服手動 OTP：啟動前由使用者輸入。
    Typed(String),
    /// 繁中服手動 OTP：在登入確認對話框按「登入」前輸入，跟 reCAPTCHA token 一起交回；使用者在對話框停多久都不會
    /// 拿到過期的 code。
    InDialog,
    /// 本機 TOTP 金鑰（已解碼）。
    Auto(Vec<u8>),
}

impl OtpSource {
    fn in_dialog(&self) -> bool {
        matches!(self, OtpSource::InDialog)
    }

    /// 自動 OTP 以 `unix_seconds` 產生；繁中服手動 OTP 用對話框交回的 `from_dialog`。
    fn code(&self, from_dialog: Option<&str>, unix_seconds: u64) -> AppResult<Option<String>> {
        match self {
            OtpSource::None => Ok(None),
            OtpSource::Typed(code) => Ok(Some(code.clone())),
            OtpSource::InDialog => from_dialog.map(|c| Some(c.to_string())).ok_or_else(otp_missing),
            OtpSource::Auto(key) => Ok(Some(totp::code_at(key, unix_seconds))),
        }
    }
}

fn otp_missing() -> AppError {
    AppError::NotFound {
        what: "一次性密碼".into(),
    }
}

fn resolve_otp(account: &Account, typed: Option<&str>) -> AppResult<OtpSource> {
    match account.otp {
        OtpMode::None => Ok(OtpSource::None),
        OtpMode::Manual if account.region == Region::Tw => Ok(OtpSource::InDialog),
        OtpMode::Manual => {
            let code = typed.map(str::trim).filter(|c| !c.is_empty());
            code.map(|c| OtpSource::Typed(c.to_string())).ok_or_else(otp_missing)
        }
        OtpMode::Auto => {
            let secret = secrets::load(&account.id, SecretKind::OtpSecret)?.ok_or_else(|| {
                AppError::NotFound {
                    what: "OTP 金鑰".into(),
                }
            })?;
            Ok(OtpSource::Auto(totp::parse_secret(&secret)?))
        }
    }
}

/// 繁中服先等登入確認對話框交回 token，之後才產生 OTP（同 TC 在 execute 之後才取 OTP）：使用者在對話框停多久，
/// 送出的自動 OTP 都是當下的；取消或失敗時不產生 OTP。`typed_otp` 取對話框交回的手動 OTP，`unix_now` 是產生
/// OTP 用的時間。
async fn token_then_otp<D>(
    otp: &OtpSource,
    dialog: impl Future<Output = AppResult<D>>,
    typed_otp: impl FnOnce(&D) -> Option<&str>,
    unix_now: impl FnOnce() -> u64,
) -> AppResult<(D, Option<String>)> {
    let dialog = dialog.await?;
    let code = otp.code(typed_otp(&dialog), unix_now())?;
    Ok((dialog, code))
}

/// 啟動前的同步檢查：帳號、遊戲路徑與執行檔，以及 dirty 標記（存在即拒絕，主 note「dirty 標記」）。
fn launch_target(paths: &AppPaths, config: &Config, account_id: &str) -> AppResult<(Account, PathBuf)> {
    let account = find_account(config, account_id)?.clone();
    let region = account.region;
    let install = config
        .games
        .get(region)
        .ok_or(AppError::GameNotConfigured { region })?
        .to_path_buf();
    let game_exe = crate::game::game_exe(&install);
    if !game_exe.is_file() {
        return Err(AppError::GameFileMissing {
            path: game_exe.display().to_string(),
        });
    }
    dirty::ensure_clean(paths, region, &install)?;
    Ok((account, install))
}

/// 登入後的 ACT 連動（protocols §6.2）要做哪些事。
#[derive(Debug, PartialEq, Eq)]
struct ActFollow {
    /// 寫 ACT 連動指令檔，讓 ACT 切到這個遊戲。
    write_target: bool,
    /// 開該區服的 ACT。
    start_act: bool,
}

/// 帳號沒勾「ACT 跟這個帳號」時兩件都不做：同服多開時小號不能把指令檔的目標從主號搶走，也不替它開 ACT。
/// 有勾時一律寫指令檔，該區服的 ACT 設定「登入後自動開啟」才開 ACT。
fn act_follow(account: &Account, act: &ActSettings) -> ActFollow {
    ActFollow {
        write_target: account.act_follow,
        start_act: account.act_follow && act.launch_with_game,
    }
}

/// [`launch_account`] 的結果：回給前端的 `outcome`，以及仍在背景觀察提早結束的 `early_exit`。
pub struct Launched {
    pub outcome: LaunchOutcome,
    pub early_exit: EarlyExitWatch,
}

/// 遊戲開出後在背景觀察是否提早結束（`dalamud::watch_early_exit`，可能超過客戶端實例上限）。`launch_account`
/// 不等它：遊戲一開出就記錄執行狀態、寫 ACT 連動、開 ACT 並回傳成功（實測觀察要 5 秒，遊戲視窗早就出現了）。
pub struct EarlyExitWatch {
    account_id: String,
    profile_id: Option<String>,
    process: GameProcess,
    watch: JoinHandle<AppResult<()>>,
}

impl EarlyExitWatch {
    /// 從現在（遊戲剛開出）開始觀察。
    fn start(account_id: &str, profile_id: Option<String>, process: GameProcess) -> Self {
        Self {
            account_id: account_id.to_string(),
            profile_id,
            process,
            watch: tokio::spawn(dalamud::watch_early_exit(process)),
        }
    }

    /// 等觀察結束。遊戲提早結束時撤銷這次啟動寫入的佔用與帳號執行紀錄（同步回錯時兩者都不會寫），再以
    /// `on_exit(帳號 id, 錯誤)` 通知，錯誤與同步回錯時相同。呼叫端在啟動的後續步驟（含收起視窗）都做完後才呼叫，
    /// 撤銷與通知一定在那之後。ACT 連動指令檔由 `ActTargets` 原本就在等的遊戲結束切回；已開起的 ACT 不關
    /// （protocols §3：不結束 ACT 程序）。
    pub async fn finish(self, paths: &AppPaths, on_exit: impl FnOnce(&str, &AppError)) {
        let Self {
            account_id,
            profile_id,
            process,
            watch,
        } = self;
        let watched = watch.await.unwrap_or_else(|e| {
            // 觀察本身沒跑完（panic）：不知道遊戲是否提早結束，不撤銷也不通知；背景沒有畫面可回報，只記 log。
            eprintln!("背景觀察遊戲是否提早結束時失敗：{e}");
            Ok(())
        });
        if let Err(e) = watched {
            revoke_running(paths, &account_id, profile_id.as_deref(), &process);
            on_exit(&account_id, &e);
        }
    }
}

/// 撤銷這次啟動寫入的佔用與帳號執行紀錄。失敗只記 log：紀錄裡的遊戲已結束，讀取端核對 pid 與建立時間，本來就
/// 不會把它當成執行中或佔用中。
fn revoke_running(paths: &AppPaths, account_id: &str, profile_id: Option<&str>, process: &GameProcess) {
    if let Some(profile_id) = profile_id
        && let Err(e) = dalamud::unmark_running(paths, profile_id, process)
    {
        eprintln!("遊戲提早結束，撤銷 Dalamud 設定檔佔用紀錄失敗：{e}");
    }
    if let Err(e) = dalamud::unmark_account_running(paths, account_id, process) {
        eprintln!("遊戲提早結束，撤銷帳號執行紀錄失敗：{e}");
    }
}

/// 呼叫端須持有 app-wide launch mutex。準備衛月階段可被 `cancel` 取消。版本檢查取得 patch list 時存進
/// `pending` 並回 [`AppError::UpdateRequired`]。`prepared_cache` 是登入前先準備好的衛月，條件相符時沿用。
#[allow(clippy::too_many_arguments)] // quality-allow: Tauri 指令的各項共用狀態逐一傳入，與 AppState 欄位一一對應
pub async fn launch_account(
    app: &AppHandle,
    paths: &AppPaths,
    pending: &PendingUpdates,
    act_targets: &ActTargets,
    prepared_cache: &PreparedCache,
    account_id: &str,
    options: LaunchOptions,
    cancel: &CancelToken,
) -> AppResult<Launched> {
    let config = store::load_config(paths)?;
    let (account, install) = launch_target(paths, &config, account_id)?;
    let region = account.region;
    let game_exe = crate::game::game_exe(&install);
    let extra_args = crate::game::parse_extra_args(&account.extra_args)?;

    let profile = match (&account.profile_id, options.without_dalamud) {
        (Some(id), false) => {
            let profile = find_profile(&config, id)?.clone();
            if profile.region() != region {
                return Err(AppError::ProfileRegionMismatch);
            }
            // 共用前要先確認（protocols §3）；前端在登入前已問過，這裡再核對一次。
            if !profile.share_confirmed && dalamud::profile_in_use(paths, &profile.id)? {
                return Err(AppError::ProfileInUse {
                    profile: profile.name.clone(),
                });
            }
            Some(profile)
        }
        _ => None,
    };

    let password = resolve_password(options.password.as_deref(), || {
        secrets::load(&account.id, SecretKind::Password)
    })?;
    let otp = resolve_otp(&account, options.otp.as_deref())?;
    // 國際服照舊在這裡產生 OTP；繁中服等登入確認對話框交回 token 才產生（token_then_otp）。
    let global_otp = match region {
        Region::Global => otp.code(None, totp::now())?,
        Region::Tw => None,
    };

    if region == Region::Global {
        emit_stage(app, account_id, "檢查國際服伺服器狀態");
        if global::fetch_gate_status().await == global::ServerStatus::Maintenance {
            return Err(AppError::ServerMaintenance { region });
        }
    }

    let prepared;
    let game_args = match region {
        Region::Global => {
            emit_stage(app, account_id, "檢查國際服 boot 版本");
            global::check_boot(&install).await?;
            // Lease 只用於衛月判定與 OAuth Referer。不帶衛月時不等 goatcorp（實測回應可慢到 30 秒），
            // Referer 直接用 XL 的 fallback frontier（protocols §1.4）。
            let lease = if profile.is_some() {
                emit_stage(app, account_id, "取得衛月 Lease");
                Some(global::fetch_lease().await)
            } else {
                None
            };
            emit_stage(app, account_id, "登入國際服");
            let fallback = global::Lease {
                frontier_url: FALLBACK_FRONTIER_URL.to_string(),
                flags: 0,
            };
            let login_lease = match &lease {
                Some(Ok(l)) => l,
                _ => &fallback,
            };
            let checked =
                global::login(login_lease, &install, &account.username, &password, global_otp.as_deref())
                    .await?;
            let session = update::require_up_to_date(pending, region, &install, checked)?;
            // 衛月 Lease 判定放在版本檢查之後：goatcorp 停用衛月或取不到 Lease 時仍要能走到遊戲更新
            // （驗收「綁 profile＋舊遊戲時能走到更新」）。取不到 Lease 同樣不注入（fail closed）。
            match &lease {
                Some(Ok(l)) if l.dalamud_disabled() => return Err(AppError::DalamudRemoteDisabled),
                Some(Err(_)) => return Err(AppError::DalamudLeaseUnavailable),
                Some(Ok(_)) | None => {}
            }
            let check = options.version_check;
            prepared = prepare_profile(app, paths, prepared_cache, account_id, profile.as_ref(), &install, check, cancel)
                .await?;
            // 參數以當下的 tick 加密（protocols §1.2），在準備衛月之後、啟動之前才產生。
            let game_version = crate::game::game_version(&install)?;
            global::launch_args(&session, &game_version, account.language, &extra_args)
        }
        Region::Tw => {
            emit_stage(app, account_id, &format!("檢查{region}遊戲版本"));
            let checked = tw::login::check_version(&install).await?;
            update::require_up_to_date(pending, region, &install, checked)?;
            let check = options.version_check;
            prepared = prepare_profile(app, paths, prepared_cache, account_id, profile.as_ref(), &install, check, cancel)
                .await?;
            emit_stage(app, account_id, &format!("{region} 登入確認（請在跳出的視窗按「登入」）"));
            let label = if account.name.is_empty() { &account.username } else { &account.name };
            let (dialog, code) = token_then_otp(
                &otp,
                tw::acquire_token(app, label, otp.in_dialog()),
                tw::LoginDialog::typed_otp,
                totp::now,
            )
            .await?;
            emit_stage(app, account_id, &format!("登入{region}"));
            let checked = tw::login(&install, &account.username, &password, code.as_deref(), dialog.token()).await;
            // 登入流程（launcherLogin → 版本檢查 → launcherSession）跑完才關對話框，同 TC；期間頁面顯示「登入中…」。
            drop(dialog);
            let checked = checked?;
            let session = update::require_up_to_date(pending, region, &install, checked)?;
            tw::launch_args(&session, &extra_args)
        }
    };

    emit_stage(app, account_id, "啟動遊戲");
    let (process, dalamud_version) = match (&profile, prepared) {
        (Some(p), Some(prepared)) => {
            let profile_paths = dalamud::ensure_profile_paths(paths, p)?;
            let language = match region {
                Region::Global => account.language.code(),
                Region::Tw => TW_DALAMUD_LANGUAGE,
            };
            let request = InjectRequest {
                injector_dir: prepared.injector_dir,
                runtime_dir: prepared.runtime_dir,
                assets_dir: prepared.assets_dir,
                profile: profile_paths,
                game_exe,
                game_args,
                language,
                delay_ms: p.injection_delay_ms,
                mode: prepared.inject_mode,
                plugins: options.plugins,
            };
            let process = dalamud::launch_with_dalamud(&request).await?;
            dalamud::mark_running(paths, &p.id, &process)?;
            (process, Some(prepared.version))
        }
        _ => (dalamud::launch_plain(&game_exe, &game_args).await?, None),
    };
    let early_exit = EarlyExitWatch::start(account_id, profile.as_ref().map(|p| p.id.clone()), process);
    // 帳號執行紀錄只供畫面顯示；遊戲已經開起來，寫失敗不能回報成啟動失敗（使用者會重試、開出第二個遊戲）。
    let _ = dalamud::mark_account_running(paths, account_id, &process);
    // ACT 連動（protocols §6.2）：同上，寫不成只回警告。先寫再開 ACT，ACT 一開外掛就讀得到這次的遊戲。
    // 寫成後背景等這個遊戲結束，結束時切回同服仍開著、有勾的帳號（等待與切回的錯誤只記 log）。
    let act = config.act.get(region);
    let follow = act_follow(&account, act);
    let act_target_warning = if follow.write_target {
        act_targets
            .follow_login(region, &process)
            .err()
            .map(|e| format!("ACT 連動沒有更新（{e}），ACT 不會自動切到這個遊戲"))
    } else {
        None
    };

    // 遊戲已開出：ACT 開不起來（含 UAC 被拒）只回警告，不能回報成啟動失敗（同上，會開出第二個遊戲）。
    let act_warning = if follow.start_act {
        emit_stage(app, account_id, "開啟 ACT");
        crate::act::start(act.path.clone()).await.err().map(|e| e.to_string())
    } else {
        None
    };

    Ok(Launched {
        outcome: LaunchOutcome {
            pid: process.pid,
            dalamud_version,
            act_warning,
            act_target_warning,
        },
        early_exit,
    })
}

/// 獨立安裝（不啟動）時進度事件的 `accountId`。
pub fn install_progress_id(region: Region) -> &'static str {
    match region {
        Region::Global => "install:global",
        Region::Tw => "install:tw",
    }
}

/// 下載安裝衛月，進度以 `progress_id`（帳號 id 或 [`install_progress_id`]）送出。
/// 呼叫端須持有 app-wide launch mutex。
pub async fn prepare_dalamud(
    app: &AppHandle,
    paths: &AppPaths,
    progress_id: &str,
    channel: &DalamudChannel,
    install: &Path,
    check: GameVersionCheck,
    cancel: &CancelToken,
) -> AppResult<PreparedDalamud> {
    let app = app.clone();
    let id = progress_id.to_string();
    let report = move |p: Progress| {
        let _ = app.emit(
            PROGRESS_EVENT,
            ProgressPayload {
                account_id: &id,
                stage: &p.stage,
                done: p.done,
                total: p.total,
            },
        ); // 進度事件只影響畫面
    };
    dalamud::prepare(paths, channel, install, check, &report, cancel).await
}

/// 帳號綁了衛月 profile 時才準備衛月；登入前剛準備好的結果可沿用就不再準備（`PreparedCache::reuse_or_prepare`）。
#[allow(clippy::too_many_arguments)] // quality-allow: 與 launch_account 相同，共用狀態與本次登入選項逐一傳入
async fn prepare_profile(
    app: &AppHandle,
    paths: &AppPaths,
    prepared_cache: &PreparedCache,
    account_id: &str,
    profile: Option<&Profile>,
    install: &Path,
    check: GameVersionCheck,
    cancel: &CancelToken,
) -> AppResult<Option<PreparedDalamud>> {
    let Some(p) = profile else {
        return Ok(None);
    };
    let prepare = prepare_dalamud(app, paths, account_id, &p.channel, install, check, cancel);
    Ok(Some(prepared_cache.reuse_or_prepare(&p.channel, install, prepare).await?))
}

#[cfg(test)]
mod tests {
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::patch::list::Repo;
    use crate::patch::running::test_support::GAME_PROCESSES;
    use crate::patch::update::test_support::{
        ACCOUNT_ID, EX1_DAT, EX1_VER, EX1_VERSION, Stub, TwInstall, made_entry, serve_files,
        update_over_http,
    };
    use crate::patch::zipatch::test_support::{Patch, add_data, on_op_applied};

    fn stored(value: Option<&str>) -> impl FnOnce() -> AppResult<Option<String>> {
        let value = value.map(str::to_string);
        move || Ok(value)
    }

    fn never_loaded() -> AppResult<Option<String>> {
        panic!("已輸入密碼時不應讀取已存密碼")
    }

    #[test]
    fn typed_password_wins_without_touching_stored() {
        assert_eq!(resolve_password(Some("typed"), never_loaded).expect("typed"), "typed");
        // 不 trim：密碼可含空白。
        assert_eq!(resolve_password(Some(" p w "), never_loaded).expect("spaces"), " p w ");
    }

    #[test]
    fn empty_typed_password_falls_back_to_stored() {
        assert_eq!(resolve_password(Some(""), stored(Some("saved"))).expect("stored"), "saved");
        assert_eq!(resolve_password(None, stored(Some("saved"))).expect("stored"), "saved");
    }

    #[test]
    fn no_password_anywhere_is_password_required() {
        assert!(matches!(resolve_password(None, stored(None)), Err(AppError::PasswordRequired)));
        assert!(matches!(
            resolve_password(Some(""), stored(Some(""))),
            Err(AppError::PasswordRequired)
        ));
        assert_eq!(AppError::PasswordRequired.to_string(), "請輸入密碼");
    }

    #[test]
    fn stored_lookup_error_propagates() {
        let failing = || Err(AppError::Secret { context: "讀取憑證".into() });
        assert!(matches!(resolve_password(None, failing), Err(AppError::Secret { .. })));
    }

    #[test]
    fn launch_options_password_is_optional() {
        let legacy: LaunchOptions =
            serde_json::from_value(serde_json::json!({"otp": null})).expect("legacy");
        assert!(legacy.password.is_none());
        let typed: LaunchOptions =
            serde_json::from_value(serde_json::json!({"otp": null, "password": "pw"})).expect("typed");
        assert_eq!(typed.password.as_deref(), Some("pw"));
    }

    /// RFC 6238 附錄 B 的 SHA1 金鑰。
    const TOTP_KEY: &[u8] = b"12345678901234567890";

    /// 繁中服自動 OTP 等對話框交回 token 才產生，用的是 token 之後的時間。
    #[tokio::test]
    async fn tw_auto_otp_is_generated_after_token() {
        let order = std::sync::Mutex::new(Vec::new());
        let source = OtpSource::Auto(TOTP_KEY.to_vec());
        let dialog = async {
            order.lock().expect("lock").push("token");
            Ok::<(), AppError>(())
        };
        let now = || {
            order.lock().expect("lock").push("otp");
            60
        };
        let (_, code) = token_then_otp(&source, dialog, |_: &()| None, now).await.expect("otp");
        assert_eq!(*order.lock().expect("lock"), ["token", "otp"]);
        assert_eq!(code, Some(totp::code_at(TOTP_KEY, 60)));
    }

    #[tokio::test]
    async fn tw_cancelled_dialog_generates_no_otp() {
        let source = OtpSource::Auto(TOTP_KEY.to_vec());
        let dialog = async { Err::<(), AppError>(AppError::RecaptchaCancelled) };
        let result = token_then_otp(&source, dialog, |_: &()| None, || panic!("取消時不應產生 OTP")).await;
        assert!(matches!(result, Err(AppError::RecaptchaCancelled)));
    }

    /// 繁中服手動 OTP 不用啟動前輸入的 code，改用對話框在按「登入」時交回的。
    #[tokio::test]
    async fn tw_manual_otp_comes_from_dialog() {
        let account = Account {
            id: "a1".into(),
            name: "台服".into(),
            region: Region::Tw,
            username: "user@example.com".into(),
            otp: OtpMode::Manual,
            profile_id: None,
            language: Default::default(),
            remember_password: false,
            extra_args: String::new(),
            act_follow: true,
        };
        let source = resolve_otp(&account, Some("111111")).expect("source");
        assert!(source.in_dialog());
        let dialog = async { Ok::<&str, AppError>("222222") };
        let (_, code) = token_then_otp(&source, dialog, |typed: &&str| Some(*typed), || 0).await.expect("otp");
        assert_eq!(code.as_deref(), Some("222222"));
    }

    #[test]
    fn launch_is_refused_while_install_is_dirty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().join("appdata"));
        let install = dir.path().join("FFXIV TC");
        let game_exe = crate::game::game_exe(&install);
        std::fs::create_dir_all(game_exe.parent().expect("parent")).expect("mkdir");
        std::fs::write(&game_exe, b"exe").expect("exe");
        let mut config = Config::default();
        config.games.set(Region::Tw, Some(install.clone()));
        config.accounts.push(Account {
            id: "a1".into(),
            name: "台服".into(),
            region: Region::Tw,
            username: "user@example.com".into(),
            otp: OtpMode::None,
            profile_id: None,
            language: Default::default(),
            remember_password: false,
            extra_args: String::new(),
            act_follow: true,
        });

        let (account, target) = launch_target(&paths, &config, "a1").expect("clean");
        assert_eq!(account.id, "a1");
        assert_eq!(target, install);

        dirty::mark(&paths, &install).expect("mark");
        assert!(matches!(
            launch_target(&paths, &config, "a1"),
            Err(AppError::PatchDirty { region: Region::Tw })
        ));

        dirty::clear(&paths, &install).expect("clear");
        assert!(launch_target(&paths, &config, "a1").is_ok());
    }

    #[test]
    fn act_follows_only_accounts_that_opt_in() {
        let account = |act_follow| Account {
            id: "a1".into(),
            name: "台服".into(),
            region: Region::Tw,
            username: "user@example.com".into(),
            otp: OtpMode::None,
            profile_id: None,
            language: Default::default(),
            remember_password: false,
            extra_args: String::new(),
            act_follow,
        };
        let act = |launch_with_game| ActSettings {
            path: None,
            launch_with_game,
        };
        let follow = |write_target, start_act| ActFollow { write_target, start_act };

        assert_eq!(act_follow(&account(true), &act(true)), follow(true, true));
        assert_eq!(act_follow(&account(true), &act(false)), follow(true, false));
        // 沒勾：區服設定開了自動開啟也不開 ACT、不寫指令檔。
        assert_eq!(act_follow(&account(false), &act(true)), follow(false, false));
        assert_eq!(act_follow(&account(false), &act(false)), follow(false, false));
    }

    // ── 背景觀察提早結束 ──────────────────────────────────────────────────────

    /// 遊戲替身：waitfor 等一個不會出現的訊號（最多 120 秒）。名稱不是遊戲執行檔，不必取 `GAME_PROCESSES`。
    fn stand_in_game() -> (Child, GameProcess) {
        let system_root = std::env::var_os("SystemRoot").expect("SystemRoot");
        let child = Command::new(Path::new(&system_root).join(r"System32\waitfor.exe"))
            .args(["/t", "120", &format!("xlm{}", uuid::Uuid::new_v4().simple())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start stand-in game");
        let game = dalamud::process_identity(child.id())
            .expect("identity")
            .expect("stand-in alive");
        (child, game)
    }

    fn profile_records(paths: &AppPaths) -> std::collections::BTreeMap<String, Vec<GameProcess>> {
        serde_json::from_slice(&std::fs::read(paths.profiles_running_file()).expect("profiles_running.json"))
            .expect("parse")
    }

    /// 回傳成功之後遊戲才結束：撤銷這次的佔用與帳號執行紀錄（別的程序的不動），並以同步回錯時的錯誤通知。
    #[tokio::test]
    async fn early_exit_after_success_revokes_running_state_and_notifies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().to_path_buf());
        let me = dalamud::process_identity(std::process::id())
            .expect("identity")
            .expect("current process is alive");
        let (mut child, game) = stand_in_game();
        dalamud::mark_running(&paths, "p1", &game).expect("mark profile");
        dalamud::mark_running(&paths, "p2", &me).expect("mark other profile");
        dalamud::mark_account_running(&paths, "a1", &game).expect("mark account");
        dalamud::mark_account_running(&paths, "a2", &me).expect("mark other account");

        let watch = EarlyExitWatch::start("a1", Some("p1".into()), game);
        child.kill().expect("kill");
        child.wait().expect("reap");
        let mut notified = None;
        watch
            .finish(&paths, |account_id, e| notified = Some((account_id.to_string(), e.to_string())))
            .await;

        assert_eq!(notified, Some(("a1".to_string(), AppError::GameExitedEarly.to_string())));
        assert_eq!(profile_records(&paths).into_keys().collect::<Vec<_>>(), ["p2"]);
        let accounts = dalamud::account_processes(&paths).expect("accounts");
        assert_eq!(accounts.into_keys().collect::<Vec<_>>(), ["a2"]);
    }

    /// 觀察期間遊戲一直在：不通知、紀錄不動。
    #[tokio::test]
    async fn surviving_game_keeps_running_state_without_notice() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().to_path_buf());
        let (mut child, game) = stand_in_game();
        dalamud::mark_running(&paths, "p1", &game).expect("mark profile");
        dalamud::mark_account_running(&paths, "a1", &game).expect("mark account");

        // 5 秒的時間窗以「已撐過」的結果代替（時間窗本身見 runner 的測試）。
        let watch = EarlyExitWatch {
            account_id: "a1".into(),
            profile_id: Some("p1".into()),
            process: game,
            watch: tokio::spawn(async { Ok(()) }),
        };
        watch.finish(&paths, |_, e| panic!("不應通知：{e}")).await;

        assert_eq!(profile_records(&paths)["p1"], [game]);
        assert_eq!(dalamud::running_accounts(&paths).expect("accounts"), ["a1"]);
        child.kill().expect("kill");
        child.wait().expect("reap");
    }

    // ── 套用中被殺（子程序）──────────────────────────────────────────────────

    /// 子程序角色：值為父程序以 [`TwInstall::create`] 建好的測試根目錄。
    const KILL_CHILD_ROOT: &str = "XLM_TEST_KILLED_DURING_APPLY_ROOT";
    /// 子程序的套用執行緒停在同步點時建立的訊號檔（相對測試根目錄）。
    const APPLYING_SIGNAL: &str = "applying";
    const KILL_TEST: &str = "launch::tests::killed_during_apply_refuses_second_update_and_launch";
    /// [`two_write_patch`] 在 stub 上的路徑。
    const TWO_WRITE_URL_PATH: &str = "/game/ex1/00000000/D2026.08.07.0000.0000.patch";
    /// [`two_write_patch`] 第一個動作寫入 [`EX1_DAT`] 的內容。
    const FIRST_WRITE: [u8; 128] = [0x11; 128];

    /// 兩個寫入動作的 ex1 patch：SQPK/A 先寫 `030100.win32.dat0`（[`FIRST_WRITE`]），再寫 `.dat1`。
    fn two_write_patch() -> Vec<u8> {
        Patch::new()
            .sqpk(b'A', &add_data(0x03, 0x0100, 0, 0, &FIRST_WRITE, 0))
            .sqpk(b'A', &add_data(0x03, 0x0100, 1, 0, &[0x22; 128], 0))
            .end()
    }

    /// 本機 stub 供 [`two_write_patch`]（ex1 [`EX1_VERSION`]，長度與 SHA1 由內容算出），pending 只有這一筆。
    fn pending_two_write_patch(fx: &TwInstall) -> (Stub, PendingUpdates) {
        let patch = two_write_patch();
        let stub = serve_files(vec![(TWO_WRITE_URL_PATH, patch.clone())]);
        let entry = made_entry(&stub, TWO_WRITE_URL_PATH, Repo::Expansion(1), EX1_VERSION, &patch);
        let pending = fx.pending(vec![entry]);
        (stub, pending)
    }

    /// 驗收「套用中殺程序後，第二次更新與啟動皆被拒，`.ver` 未被改寫」。測試 exe 以子程序重跑本測試（環境
    /// 變數 [`KILL_CHILD_ROOT`]）：子程序走 `update_game` 的本體（`update_over_http`：台服程序內套用，下載 client
    /// 換成接受本機 http stub 的，stub 供 [`two_write_patch`]），
    /// 由 zipatch 的測試同步點讓套用執行緒寫完第一個動作後停住（第二個動作、`sync_all` 與 `ex1.ver` 都還沒做），
    /// 父程序以 `TerminateProcess` 結束它。之後安裝只多了第一筆寫入、`ex1.ver` 仍是更新前的值、dirty 標記仍在；
    /// 同一份設定與安裝上的更新與啟動都在第一道檢查被拒，安裝內容不再變動。子程序的套用會走到遊戲執行判定，
    /// 父程序在它執行期間持有 `GAME_PROCESSES` 讀鎖（同程序的其他測試不會在這段時間啟動或結束替身）。
    #[tokio::test(flavor = "multi_thread")]
    async fn killed_during_apply_refuses_second_update_and_launch() {
        if let Some(root) = std::env::var_os(KILL_CHILD_ROOT) {
            return apply_until_killed(Path::new(&root)).await;
        }
        let _processes = GAME_PROCESSES.read().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let fx = TwInstall::create(dir.path());
        let ver_before = fx.read(EX1_VER);
        let before = fx.snapshot();
        let child = Command::new(std::env::current_exe().expect("current exe"))
            .args([KILL_TEST, "--exact", "--nocapture", "--test-threads=1"])
            .env(KILL_CHILD_ROOT, dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn child");
        kill_when_applying(child, &dir.path().join(APPLYING_SIGNAL));

        // 停在寫入途中：只有第一個動作寫入，被中斷的 patch 不推進 `.ver`，標記仍在。
        assert_eq!(fx.read(EX1_VER), ver_before, "被中斷的 patch 不推進 .ver");
        assert!(dirty::is_dirty(&fx.paths, &fx.install).expect("dirty"), "被殺後標記仍在");
        let killed = fx.snapshot();
        let mut partial = before;
        partial.push((PathBuf::from(EX1_DAT), Some(FIRST_WRITE.to_vec())));
        partial.sort();
        assert_eq!(killed, partial, "只有第一個動作（.dat0）寫入，第二個（.dat1）未執行");

        // 第二次更新：下載與寫入之前就被拒。
        let (stub, pending) = pending_two_write_patch(&fx);
        let cancel = dalamud::DownloadCancel::default();
        let err = update::update_game(&fx.paths, &pending, Region::Tw, &|_| {}, &cancel.token())
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::PatchDirty { region: Region::Tw }), "{err}");
        assert!(stub.requests().is_empty(), "{:?}", stub.requests());

        // 啟動：launch_account 的第一道檢查同樣拒絕。
        let config = store::load_config(&fx.paths).expect("config");
        assert!(matches!(
            launch_target(&fx.paths, &config, ACCOUNT_ID),
            Err(AppError::PatchDirty { region: Region::Tw })
        ));

        assert_eq!(fx.snapshot(), killed, "被拒後安裝內容（含 .ver）不再變動");
    }

    /// 子程序：設定同步點後走實際更新；套用執行緒寫完第一個動作時建立訊號檔並停住，等父程序結束本程序。
    async fn apply_until_killed(root: &Path) {
        let fx = TwInstall::open(root);
        let (_stub, pending) = pending_two_write_patch(&fx);
        let signal = root.join(APPLYING_SIGNAL);
        on_op_applied(move || {
            std::fs::write(&signal, b"").expect("signal");
            loop {
                std::thread::park();
            }
        });
        let cancel = dalamud::DownloadCancel::default();
        let result =
            update_over_http(&fx.paths, &pending, Region::Tw, &|_| {}, &cancel.token()).await;
        panic!("子程序應在套用中被結束，更新卻回傳 {result:?}");
    }

    /// 等子程序建立訊號檔（套用執行緒已停在同步點）後結束它並回收；子程序先結束或逾時即失敗（附子程序輸出）。
    fn kill_when_applying(mut child: Child, signal: &Path) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !signal.exists() {
            let exited = child.try_wait().expect("try_wait");
            if exited.is_some() || Instant::now() > deadline {
                let _ = child.kill();
                let output = child.wait_with_output().expect("child output");
                panic!(
                    "子程序未停在套用同步點（{}）\n{}\n{}",
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        child.kill().expect("kill child");
        child.wait().expect("wait child");
    }
}
