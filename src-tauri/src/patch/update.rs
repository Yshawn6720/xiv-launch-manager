//! 遊戲更新編排（主 note「設計」）：版本檢查結果（pending）、patch 下載與核對、dirty 標記、
//! 程序內套用（台服；國際服安裝目前程序就能修改時）與國際服提權 helper（安裝受保護時）。
//!
//! 不變量：
//! - pending 只存解析後的清單與安裝根目錄，不存 UID／SID。
//! - [`update_game`] 的呼叫端持有 app-wide launch mutex 到回傳為止（國際服經 helper 時含 UAC 與 helper 執行）。
//! - 第一筆寫入遊戲目錄前寫 dirty 標記並落盤；只有確定沒寫壞遊戲目錄的結果才清除。
//! - 套用前的失敗（下載、核對、取消）只刪本次的 patch 檔，不留標記、不提示修復。
//! - 一開始套用，pending 就作廢（版本可能已推進）：之後要重新檢查（台服）或重新登入（國際服）。

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use reqwest::Url;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::dalamud::{self, CancelToken, Progress, ProgressFn};
use crate::error::{AppError, AppResult};
use crate::patch::access::{self, AccessToken};
use crate::patch::apply::{self, ApplyFailure, ApplyProgress};
use crate::patch::dirty;
use crate::patch::helper::{self, HelperExit};
use crate::patch::job::{self, Job, JobPatch};
use crate::patch::list::{self, PatchEntry, Repo};
use crate::patch::running;
use crate::region::Region;
use crate::store::{self, AppPaths};
use crate::tw;

/// patch 下載用本專案自己的 User-Agent（同 protocols §1.4 的 Lease 請求）。
const USER_AGENT: &str = concat!("xiv-launch-manager/", env!("CARGO_PKG_VERSION"));
const DOWNLOAD_STAGE: &str = "下載遊戲 patch";
/// 轉址次數上限（同 reqwest 預設）。
const MAX_REDIRECTS: usize = 10;
/// 輪詢 helper 進度檔的間隔。
const HELPER_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 版本檢查回應中的 patch list 原文（protocols §5.1），尚未解析。只有 Content-Type 與 body：
/// 國際服 session 註冊的 UID 不放進來。
#[derive(Clone, PartialEq, Eq)]
pub struct RawPatchList {
    pub content_type: String,
    pub body: Vec<u8>,
}

/// 不印 body：國際服清單來自含 SID 的請求，一律不回顯清單內容（同 `patch::list`）。
impl fmt::Debug for RawPatchList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawPatchList")
            .field("content_type", &self.content_type)
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// 遊戲版本檢查（國際服 session 註冊、台服版本檢查）的結果。
#[derive(Debug)]
pub enum VersionCheck<T> {
    /// 無需更新：國際服帶登入結果，台服版本檢查為 `()`。
    UpToDate(T),
    /// 需要更新，帶清單原文。
    PatchList(RawPatchList),
}

/// 更新判斷用的 `game\ffxivgame.ver`（主 note「版本來源」）：缺失或空白即
/// [`AppError::VersionFileMissing`]，不以 `BASE_GAME_VERSION` 兜底送版本檢查或 session 註冊。
/// 啟動參數、衛月相容等其他用途仍用 `game::game_version`。
pub fn required_game_version(region: Region, install: &Path) -> AppResult<String> {
    required_ver(region, &install.join("game").join("ffxivgame.ver"))
}

/// 國際服 session 註冊 version report 需要的 `game\sqpack\exN\exN.ver`，規則同 [`required_game_version`]。
pub fn required_expansion_version(region: Region, install: &Path, n: u8) -> AppResult<String> {
    let name = format!("ex{n}");
    required_ver(
        region,
        &install
            .join("game")
            .join("sqpack")
            .join(&name)
            .join(format!("{name}.ver")),
    )
}

fn required_ver(region: Region, file: &Path) -> AppResult<String> {
    let missing = || AppError::VersionFileMissing {
        region,
        path: file.display().to_string(),
    };
    match fs::read_to_string(file) {
        Ok(text) if !text.trim().is_empty() => Ok(text.trim().to_string()),
        Ok(_) => Err(missing()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(missing()),
        Err(e) => Err(AppError::io(format!("讀取 {}", file.display()), e)),
    }
}

/// 一個區服待套用的更新：解析過的清單與取得清單時的安裝根目錄。
#[derive(Debug, Clone)]
pub struct PendingUpdate {
    pub install: PathBuf,
    pub patches: Vec<PatchEntry>,
}

impl PendingUpdate {
    pub fn summary(&self) -> PendingSummary {
        PendingSummary {
            patch_count: self.patches.len(),
            total_bytes: total_length(&self.patches),
        }
    }
}

/// 給前端的 pending 摘要。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingSummary {
    pub patch_count: usize,
    pub total_bytes: u64,
}

/// 各區服的 pending（`AppState` 欄位，只在記憶體）。
#[derive(Default)]
pub struct PendingUpdates(Mutex<PendingSlots>);

#[derive(Default)]
struct PendingSlots {
    global: Option<PendingUpdate>,
    tw: Option<PendingUpdate>,
}

impl PendingSlots {
    fn slot(&mut self, region: Region) -> &mut Option<PendingUpdate> {
        match region {
            Region::Global => &mut self.global,
            Region::Tw => &mut self.tw,
        }
    }
}

impl PendingUpdates {
    pub fn get(&self, region: Region) -> Option<PendingUpdate> {
        self.lock().slot(region).clone()
    }

    pub fn set(&self, region: Region, update: Option<PendingUpdate>) {
        *self.lock().slot(region) = update;
    }

    /// 臨界區只有賦值與 clone，持鎖時 panic 也不會留下半個狀態，poison 直接取回內容。
    fn lock(&self) -> MutexGuard<'_, PendingSlots> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// 解析清單並存成該區服的 pending。解析失敗即清掉 pending 並回該錯誤（失敗關閉：不啟動、不更新）。
pub fn store_patch_list(
    pending: &PendingUpdates,
    region: Region,
    install: &Path,
    raw: &RawPatchList,
) -> AppResult<PendingSummary> {
    match list::parse_patch_list(region, &raw.content_type, &raw.body) {
        Ok(patches) => {
            let update = PendingUpdate {
                install: install.to_path_buf(),
                patches,
            };
            let summary = update.summary();
            pending.set(region, Some(update));
            Ok(summary)
        }
        Err(e) => {
            pending.set(region, None);
            Err(e)
        }
    }
}

/// 啟動流程用：無需更新時清掉該區服的 pending 並回傳登入結果；有清單時解析並存成 pending，回
/// [`AppError::UpdateRequired`]（解析失敗回該錯誤），不繼續啟動（主 note「流程」）。
pub fn require_up_to_date<T>(
    pending: &PendingUpdates,
    region: Region,
    install: &Path,
    check: VersionCheck<T>,
) -> AppResult<T> {
    match check {
        VersionCheck::UpToDate(value) => {
            pending.set(region, None);
            Ok(value)
        }
        VersionCheck::PatchList(raw) => {
            let summary = store_patch_list(pending, region, install, &raw)?;
            Err(AppError::UpdateRequired {
                region,
                patch_count: summary.patch_count,
                total_bytes: summary.total_bytes,
            })
        }
    }
}

/// `check_game_update` 的結果。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum GameUpdateStatus {
    UpToDate,
    UpdateAvailable(PendingSummary),
    /// 國際服的清單要登入（session 註冊）才拿得到，目前沒有 pending。
    LoginRequired,
}

/// 台服不需登入，直接送版本檢查並更新 pending；國際服只回目前的 pending。
/// 台服會改寫 pending，呼叫端須持有 app-wide launch mutex。
pub async fn check_game_update(
    paths: &AppPaths,
    pending: &PendingUpdates,
    region: Region,
) -> AppResult<GameUpdateStatus> {
    match region {
        Region::Tw => {
            let install = configured_install(paths, region)?;
            match tw::login::check_version(&install).await? {
                VersionCheck::UpToDate(()) => {
                    pending.set(region, None);
                    Ok(GameUpdateStatus::UpToDate)
                }
                VersionCheck::PatchList(raw) => Ok(GameUpdateStatus::UpdateAvailable(
                    store_patch_list(pending, region, &install, &raw)?,
                )),
            }
        }
        Region::Global => Ok(match pending.get(region) {
            Some(update) => GameUpdateStatus::UpdateAvailable(update.summary()),
            None => GameUpdateStatus::LoginRequired,
        }),
    }
}

/// 單一區服的更新狀態（`get_patch_state`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegionPatchState {
    /// 設定中的安裝有 dirty 標記（拒絕啟動與更新，待使用者確認清除）。
    pub dirty: bool,
    pub pending: Option<PendingSummary>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchState {
    pub global: RegionPatchState,
    pub tw: RegionPatchState,
}

pub fn patch_state(paths: &AppPaths, pending: &PendingUpdates) -> AppResult<PatchState> {
    let config = store::load_config(paths)?;
    let region_state = |region: Region| -> AppResult<RegionPatchState> {
        let dirty = match config.games.get(region) {
            Some(install) => dirty::is_dirty(paths, install)?,
            None => false,
        };
        Ok(RegionPatchState {
            dirty,
            pending: pending.get(region).map(|update| update.summary()),
        })
    };
    Ok(PatchState {
        global: region_state(Region::Global)?,
        tw: region_state(Region::Tw)?,
    })
}

/// 使用者確認已用官方啟動器修復後，清除設定中該區服安裝的 dirty 標記。呼叫端須持有 app-wide launch mutex。
pub fn clear_dirty(paths: &AppPaths, region: Region) -> AppResult<()> {
    dirty::clear(paths, &configured_install(paths, region)?)
}

/// `update_game` 經 Channel 送給前端的進度；`patch_index` 從 0 起算。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum GameUpdateProgress {
    /// 下載第 `patch_index` 個 patch；`done`／`total` 為該檔 bytes。
    Download {
        patch_index: usize,
        patch_count: usize,
        done: u64,
        total: u64,
    },
    /// 核對剛下載的 patch（區塊 SHA1）。
    Verify { patch_index: usize, patch_count: usize },
    /// 國際服安裝受保護、經提權 helper 套用時：等待 UAC 同意與更新程式啟動。
    Elevate,
    /// 套用第 `patch_index` 個 patch；`done`／`total` 為該 patch 已處理的 bytes。
    Apply {
        patch_index: usize,
        patch_count: usize,
        done: u64,
        total: u64,
    },
}

pub type GameUpdateProgressFn = dyn Fn(GameUpdateProgress) + Send + Sync;

/// 下載、核對並套用該區服的 pending 更新。呼叫端須持有 app-wide launch mutex 到回傳為止（國際服經 helper
/// 時含 UAC 等待與 helper 執行，主 note「互斥」）。國際服完成後由使用者再按一次啟動重新登入（重新取得 OTP），
/// 這裡不自動登入。取消只在下載與 patch 之間生效。
pub async fn update_game(
    paths: &AppPaths,
    pending: &PendingUpdates,
    region: Region,
    progress: &GameUpdateProgressFn,
    cancel: &CancelToken,
) -> AppResult<()> {
    let client = patch_client(PatchTransport::of(region))?;
    update_with(&client, paths, pending, region, progress, cancel).await
}

/// [`update_game`] 本體；`client` 是依區服傳輸要求建立的下載 client（測試注入接受本機 http stub 的 client）。
async fn update_with(
    client: &reqwest::Client,
    paths: &AppPaths,
    pending: &PendingUpdates,
    region: Region,
    progress: &GameUpdateProgressFn,
    cancel: &CancelToken,
) -> AppResult<()> {
    let install = configured_install(paths, region)?;
    dirty::ensure_clean(paths, region, &install)?;
    let update = pending
        .get(region)
        .filter(|update| update.install == install)
        .ok_or_else(|| AppError::NotFound {
            what: format!("{region} 待套用的遊戲更新（請先檢查更新）"),
        })?;
    if running::install_in_use(&install)? {
        return Err(AppError::GameRunning);
    }
    let route = apply_route(region, || access::may_modify(&AccessToken::current()?, &install))?;

    let patches = download_all(client, paths, region, &install, &update.patches, progress, cancel).await?;
    let downloaded: Vec<PathBuf> = patches.iter().map(|p| p.path.clone()).collect();
    if let Err(e) = cancel.check().and_then(|()| dirty::mark(paths, &install)) {
        remove_files(&downloaded);
        return Err(e);
    }
    // 以下可能寫入遊戲目錄：清單可能只套用了一部分，一律作廢。
    pending.set(region, None);
    let applied = match route {
        ApplyRoute::InProcess => apply_in_process(region, &install, patches, progress, cancel).await,
        ApplyRoute::Elevated => apply_elevated(paths, &install, patches, progress).await,
    };
    match applied {
        Applied::Done => {
            remove_files(&downloaded);
            dirty::clear(paths, &install)
        }
        Applied::Clean(error) => {
            remove_files(&downloaded);
            dirty::clear(paths, &install)?;
            Err(error)
        }
        Applied::Broken { error, helper_unknown } => {
            if !helper_unknown {
                remove_files(&downloaded);
            }
            Err(error)
        }
    }
}

/// 套用方式（主 note「國際服寫入權限」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyRoute {
    /// 本程式程序內套用：台服一律如此；國際服在目前程序就能修改安裝時。
    InProcess,
    /// 經 UAC 啟動提權 helper：國際服安裝受保護（例如 Program Files）時。
    Elevated,
}

/// 只有國際服會呼叫 `may_modify`（目前程序能否修改安裝根目錄或 `game\`，[`access::may_modify`]）。能改就
/// 不提權：一般權限程序本來就能改那個安裝，交給 helper 只會讓「檢查 junction 到實際寫入」之間被換掉的子目錄
/// 把系統管理員權限的寫入導到別處。判定失敗回錯誤（尚未下載、未寫任何東西）。
fn apply_route(
    region: Region,
    may_modify: impl FnOnce() -> AppResult<bool>,
) -> AppResult<ApplyRoute> {
    Ok(match region {
        Region::Tw => ApplyRoute::InProcess,
        Region::Global if may_modify()? => ApplyRoute::InProcess,
        Region::Global => ApplyRoute::Elevated,
    })
}

/// 套用的結果分類（主 note「dirty 標記」）。
enum Applied {
    Done,
    /// 失敗的 patch 尚未寫入任何遊戲檔，之前的都已完整套用並寫好 `.ver`：清除標記。
    Clean(AppError),
    /// 可能已寫入遊戲目錄：保留標記。`helper_unknown`＝helper 是否仍在執行不明，patch 與 job 不刪。
    Broken { error: AppError, helper_unknown: bool },
}

/// 在本程式內套用（不經 helper）：台服安裝一般使用者可寫；國際服安裝目前程序可修改時也走這裡。
async fn apply_in_process(
    region: Region,
    install: &Path,
    patches: Vec<JobPatch>,
    progress: &GameUpdateProgressFn,
    cancel: &CancelToken,
) -> Applied {
    let (tx, mut rx) = watch::channel(None::<GameUpdateProgress>);
    let install = install.to_path_buf();
    let token = cancel.clone();
    let job = tokio::task::spawn_blocking(move || {
        let mut report = move |p: ApplyProgress| {
            tx.send_replace(Some(GameUpdateProgress::Apply {
                patch_index: p.patch_index,
                patch_count: p.patch_count,
                done: p.done,
                total: p.total,
            }));
        };
        apply::apply_patches(&install, &patches, &mut report, &|| token.check().is_err())
    });
    // 發送端隨 blocking 工作結束而 drop，迴圈在最後一筆進度送出後結束。
    while rx.changed().await.is_ok() {
        let latest = rx.borrow_and_update().clone();
        if let Some(p) = latest {
            progress(p);
        }
    }
    match job.await {
        Ok(Ok(())) => Applied::Done,
        Ok(Err(ApplyFailure::BeforeWrite(error))) => Applied::Clean(error),
        Ok(Err(ApplyFailure::AfterWrite(error))) => Applied::Broken {
            error: broken(region, error.to_string()),
            helper_unknown: false,
        },
        Err(_) => Applied::Broken {
            error: broken(region, "套用程序中斷".into()),
            helper_unknown: false,
        },
    }
}

/// 國際服安裝受保護（目前程序不能修改）時，經 UAC 啟動提權 helper 套用（主 note「國際服寫入權限」）。成敗只看
/// exit code；進度檔僅供顯示。
async fn apply_elevated(
    paths: &AppPaths,
    install: &Path,
    patches: Vec<JobPatch>,
    progress: &GameUpdateProgressFn,
) -> Applied {
    let job_dir = paths
        .patches_dir()
        .join(region_dir(Region::Global))
        .join(format!("job-{}", uuid::Uuid::new_v4().simple()));
    let progress_path = job::progress_path(&job_dir);
    let (job_path, job_sha256) = match prepare_job(&job_dir, install, patches) {
        Ok(written) => written,
        Err(error) => {
            remove_dir(&job_dir);
            return Applied::Clean(error);
        }
    };

    progress(GameUpdateProgress::Elevate);
    let mut elevated =
        tokio::task::spawn_blocking(move || helper::run_elevated(&job_path, &job_sha256));
    let mut ticker = tokio::time::interval(HELPER_POLL_INTERVAL);
    let mut last = None;
    let exit = loop {
        tokio::select! {
            joined = &mut elevated => break joined,
            _ = ticker.tick() => {
                if let Some(p) = read_helper_progress(&progress_path)
                    && last != Some(p)
                {
                    last = Some(p);
                    progress(GameUpdateProgress::Apply {
                        patch_index: p.patch_index,
                        patch_count: p.patch_count,
                        done: p.done,
                        total: p.total,
                    });
                }
            }
        }
    };

    let (applied, helper_exited) = match exit {
        Ok(Ok(helper_exit)) => (classify_helper_exit(helper_exit), true),
        Ok(Err(error)) => (
            Applied::Broken {
                error: broken(Region::Global, error.to_string()),
                helper_unknown: true,
            },
            false,
        ),
        Err(_) => (
            Applied::Broken {
                error: broken(Region::Global, "等待更新程式時中斷".into()),
                helper_unknown: true,
            },
            false,
        ),
    };
    if helper_exited {
        remove_dir(&job_dir);
    }
    applied
}

/// helper exit code 的分類（T5 契約：0 成功、10 寫入前中止、11 互斥被佔、12 安裝一般權限可修改、20 寫入後
/// 失敗；UAC 被拒與沒能啟動時 helper 沒有執行，遊戲目錄未被寫過）。
fn classify_helper_exit(helper_exit: HelperExit) -> Applied {
    match helper_exit {
        HelperExit::Success => Applied::Done,
        HelperExit::AbortedBeforeWrite => Applied::Clean(AppError::UpdateAborted {
            reason: "更新程式在寫入前中止：patch 核對不符、安裝路徑不符或偵測到遊戲執行".into(),
        }),
        HelperExit::InstallModifiable => Applied::Clean(AppError::UpdateAborted {
            reason: "安裝資料夾不需系統管理員權限就能修改，更新程式拒絕以系統管理員權限寫入".into(),
        }),
        HelperExit::UacDeclined => Applied::Clean(AppError::UpdateAborted {
            reason: "未同意系統管理員權限".into(),
        }),
        HelperExit::NotStarted => Applied::Clean(AppError::UpdateAborted {
            reason: "無法啟動更新程式".into(),
        }),
        // 另一個套用可能正在寫同一個安裝，保留標記。
        HelperExit::Busy => Applied::Broken {
            error: broken(Region::Global, "另一個更新程式正在套用同一個安裝".into()),
            helper_unknown: false,
        },
        HelperExit::FailedAfterWrite => Applied::Broken {
            error: broken(Region::Global, "更新程式在寫入遊戲檔後失敗".into()),
            helper_unknown: false,
        },
        HelperExit::Other(code) => Applied::Broken {
            error: broken(Region::Global, format!("更新程式異常結束（exit code {code}）")),
            helper_unknown: false,
        },
    }
}

fn broken(region: Region, reason: String) -> AppError {
    AppError::UpdateBroken { region, reason }
}

/// 建立 job 目錄與進度檔（[`job::progress_path`] 的空檔：helper 只覆寫長度為 0 的既有檔，啟動前不寫
/// 內容），再寫 job.json。
fn prepare_job(
    job_dir: &Path,
    install: &Path,
    patches: Vec<JobPatch>,
) -> AppResult<(PathBuf, String)> {
    fs::create_dir_all(job_dir).map_err(|e| AppError::io("建立更新 job 目錄", e))?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(job::progress_path(job_dir))
        .map_err(|e| AppError::io("建立更新進度檔", e))?;
    let job = Job {
        expected_install: install.to_path_buf(),
        patches,
    };
    job::write_job(job_dir, &job)
}

/// helper 進度檔（一行 JSON，整檔覆寫）。僅供顯示：讀不到、寫到一半或格式不符就略過這次輪詢。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HelperProgress {
    patch_index: usize,
    patch_count: usize,
    done: u64,
    total: u64,
}

fn read_helper_progress(path: &Path) -> Option<HelperProgress> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn configured_install(paths: &AppPaths, region: Region) -> AppResult<PathBuf> {
    store::load_config(paths)?
        .games
        .get(region)
        .map(Path::to_path_buf)
        .ok_or(AppError::GameNotConfigured { region })
}

fn region_dir(region: Region) -> &'static str {
    match region {
        Region::Global => "global",
        Region::Tw => "tw",
    }
}

/// 與 URL 路徑的 repo 段相同（protocols §5.1）。
fn repo_dir(repo: Repo) -> String {
    match repo {
        Repo::Game => "game".to_string(),
        Repo::Expansion(n) => format!("ex{n}"),
    }
}

fn total_length(entries: &[PatchEntry]) -> u64 {
    entries
        .iter()
        .fold(0u64, |sum, entry| sum.saturating_add(entry.length))
}

/// 依清單順序下載並核對所有 patch 到 `patches\<region>\<repo>\<file>`。任何失敗（含取消）刪掉本次
/// 已下載的檔再回錯誤；此階段不碰遊戲目錄與 dirty 標記。
async fn download_all(
    client: &reqwest::Client,
    paths: &AppPaths,
    region: Region,
    install: &Path,
    entries: &[PatchEntry],
    progress: &GameUpdateProgressFn,
    cancel: &CancelToken,
) -> AppResult<Vec<JobPatch>> {
    let dir = paths.patches_dir().join(region_dir(region));
    fs::create_dir_all(&dir).map_err(|e| AppError::io("建立 patch 下載目錄", e))?;
    let required = total_length(entries);
    let volumes = [
        (dir.as_path(), free_space(&dir)?),
        (install, free_space(install)?),
    ];
    if let Some((path, available)) = insufficient_space(required, &volumes) {
        return Err(AppError::InsufficientDiskSpace {
            path: path.display().to_string(),
            required,
            available,
        });
    }

    let count = entries.len();
    let mut done: Vec<JobPatch> = Vec::with_capacity(count);
    for (index, entry) in entries.iter().enumerate() {
        let dest = dir.join(repo_dir(entry.repo)).join(&entry.file_name);
        if let Err(e) = fetch_and_verify(client, entry, &dest, index, count, progress, cancel).await {
            let mut written: Vec<PathBuf> = done.into_iter().map(|p| p.path).collect();
            written.push(dest);
            remove_files(&written);
            return Err(e);
        }
        done.push(JobPatch {
            path: dest,
            repo: entry.repo,
            version_id: entry.version_id.clone(),
            length: entry.length,
            hash_block_size: entry.hash_block_size,
            hashes: entry.hashes.clone(),
        });
    }
    Ok(done)
}

async fn fetch_and_verify(
    client: &reqwest::Client,
    entry: &PatchEntry,
    dest: &Path,
    patch_index: usize,
    patch_count: usize,
    progress: &GameUpdateProgressFn,
    cancel: &CancelToken,
) -> AppResult<()> {
    let total = entry.length;
    let forward = |done: u64| {
        progress(GameUpdateProgress::Download {
            patch_index,
            patch_count,
            done,
            total,
        });
    };
    // `download_to_file` 的進度回呼須為 'static：經 watch channel 轉回這裡回報（同 apply_in_process）。
    let (tx, mut rx) = watch::channel(None::<u64>);
    let report = move |p: Progress| {
        tx.send_replace(Some(p.done));
    };
    let download = download_patch(client, &entry.url, dest, entry.length, &report, cancel);
    tokio::pin!(download);
    let downloaded = loop {
        tokio::select! {
            result = &mut download => break result,
            Ok(()) = rx.changed() => {
                let latest = *rx.borrow_and_update();
                if let Some(done) = latest {
                    forward(done);
                }
            }
        }
    };
    // 下載結束時最後一筆進度可能還沒轉出。
    if rx.has_changed().unwrap_or(false) {
        let latest = *rx.borrow_and_update();
        if let Some(done) = latest {
            forward(done);
        }
    }
    downloaded?;
    cancel.check()?;
    progress(GameUpdateProgress::Verify {
        patch_index,
        patch_count,
    });
    let (path, length, block_size, hashes) = (
        dest.to_path_buf(),
        entry.length,
        entry.hash_block_size,
        entry.hashes.clone(),
    );
    tokio::task::spawn_blocking(move || {
        let file = File::open(&path).map_err(|e| AppError::io("開啟 patch 檔", e))?;
        list::verify_blocks(file, length, block_size, &hashes)
    })
    .await
    .map_err(|_| AppError::Install {
        context: "核對 patch 檔".into(),
    })?
}

/// 下載一個 patch 到 `dest`。清單長度交給 downloader，在每個整檔回應進入配置（`set_len`）或寫入前
/// 比對 `Content-Length`（主 note「下載」）；下載完再比一次實際寫入的長度。
async fn download_patch(
    client: &reqwest::Client,
    url: &Url,
    dest: &Path,
    length: u64,
    progress: &ProgressFn,
    cancel: &CancelToken,
) -> AppResult<()> {
    let written = dalamud::download_to_file_exact(
        client,
        url.as_str(),
        dest,
        length,
        DOWNLOAD_STAGE,
        progress,
        cancel,
    )
    .await?;
    if written != length {
        return Err(AppError::PatchVerifyFailed {
            reason: "下載的長度與清單不符".into(),
        });
    }
    Ok(())
}

/// patch 下載的傳輸要求（protocols §5.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchTransport {
    /// 台服：只走 HTTPS（`https_only`，轉址也不得降級成 http）。清單是 HTTP 取得的，URL 已在解析時改成 https
    /// （`patch::list`）；被改過的清單因此只能指向官方主機經 TLS 傳來的檔案。
    HttpsOnly,
    /// 國際服：照清單的 http URL 下載（`https://patch-dl.ffxiv.com` 不通），hash 來自 HTTPS 的 session 註冊。
    AsListed,
}

impl PatchTransport {
    fn of(region: Region) -> Self {
        match region {
            Region::Tw => Self::HttpsOnly,
            Region::Global => Self::AsListed,
        }
    }
}

fn patch_client(transport: PatchTransport) -> AppResult<reqwest::Client> {
    build_patch_client(reqwest::Client::builder(), transport)
}

/// patch 下載用的 client：同 `dalamud::http_client`（HTTP/1.1 讓分段各自一條連線、只設連線與讀取逾時），
/// 另外只跟隨 [`redirect_rule`] 允許的轉址（主 note「下載」），也不要求壓縮傳輸（`Content-Length` 才會是
/// 檔案本身的長度）。與 Dalamud 下載的 client 分開，Dalamud 的行為不受影響。
fn build_patch_client(
    builder: reqwest::ClientBuilder,
    transport: PatchTransport,
) -> AppResult<reqwest::Client> {
    builder
        .user_agent(USER_AGENT)
        .http1_only()
        .no_gzip()
        .no_deflate()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .https_only(transport == PatchTransport::HttpsOnly)
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            match redirect_rule(transport, attempt.previous(), attempt.url()) {
                Ok(()) => attempt.follow(),
                Err(reason) => attempt.error(reason),
            }
        }))
        .build()
        .map_err(|e| AppError::network("建立 HTTP client", e))
}

/// 轉址規則：只跟隨與原始請求同 host 的轉址；[`PatchTransport::HttpsOnly`] 時目標也必須是 https（不降級；
/// `https_only` 也會擋，這裡明列以便測試）；次數上限同 reqwest 預設。`previous` 是已請求過的 URL，第一個
/// 是原始請求。
fn redirect_rule(
    transport: PatchTransport,
    previous: &[Url],
    next: &Url,
) -> Result<(), &'static str> {
    let same_host = previous
        .first()
        .is_some_and(|first| first.host_str() == next.host_str());
    if !same_host {
        Err("patch 下載不跟隨跨 host 的轉址")
    } else if transport == PatchTransport::HttpsOnly && next.scheme() != "https" {
        Err("patch 下載不跟隨降級成 http 的轉址")
    } else if previous.len() > MAX_REDIRECTS {
        Err("轉址次數過多")
    } else {
        Ok(())
    }
}

/// 下載碟與安裝碟各自至少要有本次 patch 總長的剩餘空間，任一不足即不開始（主 note「下載」）。
/// 回傳第一個不足的位置與其剩餘空間。
fn insufficient_space<'a>(required: u64, volumes: &[(&'a Path, u64)]) -> Option<(&'a Path, u64)> {
    volumes
        .iter()
        .copied()
        .find(|&(_, available)| available < required)
}

/// 呼叫者可用的剩餘空間（`GetDiskFreeSpaceExW`，含配額限制）。`path` 須為既有目錄。
fn free_space(path: &Path) -> AppResult<u64> {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    use windows::core::PCWSTR;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut available = 0u64;
    // SAFETY: wide 是以 0 結尾、呼叫期間存活的 UTF-16 字串；available 指向本函式存活中的區域變數，
    // 其他兩個輸出為 None。quality-allow: Win32 FFI，GetDiskFreeSpaceExW 沒有安全包裝
    let queried = unsafe {
        GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&raw mut available), None, None)
    };
    queried.map_err(|e| AppError::Io {
        context: format!("查詢 {} 的剩餘空間", path.display()),
        message: e.message(),
    })?;
    Ok(available)
}

/// 刪除本次下載的 patch 檔，以及因此變空的 repo 子目錄（`patches\<區服>\<repo>`）。
fn remove_files(files: &[PathBuf]) {
    for file in files {
        // quality-allow: best-effort 清理，刪不掉不影響更新結果，下次下載會覆寫同名檔
        let _ = fs::remove_file(file);
        if let Some(dir) = file.parent() {
            // quality-allow: remove_dir 只刪空目錄；還有其他 patch 時會失敗，正是要的結果
            let _ = fs::remove_dir(dir);
        }
    }
}

fn remove_dir(dir: &Path) {
    // quality-allow: best-effort 清理，job 目錄由一般權限建立，刪不掉時使用者可手動刪除
    let _ = fs::remove_dir_all(dir);
}

/// 測試共用（`update`、`launch`、`helper` 的測試）：台服實際素材、本機 stub server（同 `dalamud::install`
/// 測試的寫法）與台服的隔離可寫假安裝。
#[cfg(test)]
pub(crate) mod test_support {
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use reqwest::Url;
    use sha1::{Digest, Sha1};

    use super::{
        GameUpdateProgressFn, PatchTransport, PendingUpdate, PendingUpdates, build_patch_client,
        update_with,
    };
    use crate::dalamud::CancelToken;
    use crate::error::AppResult;
    use crate::patch::list::{self, PatchEntry, Repo};
    use crate::region::Region;
    use crate::store::{self, Account, AppPaths, Config, OtpMode};

    /// 接受本機 http stub 的下載 client（[`PatchTransport::AsListed`]、不經 proxy）。台服正式下載只走 HTTPS，
    /// 本機 stub 只有 http，所以台服的流程測試換用這個 client，其餘與 [`super::update_game`] 相同。
    pub(crate) fn http_client() -> reqwest::Client {
        build_patch_client(reqwest::Client::builder().no_proxy(), PatchTransport::AsListed)
            .expect("client")
    }

    /// 以 [`http_client`] 走 `update_game` 的本體。
    pub(crate) async fn update_over_http(
        paths: &AppPaths,
        pending: &PendingUpdates,
        region: Region,
        progress: &GameUpdateProgressFn,
        cancel: &CancelToken,
    ) -> AppResult<()> {
        update_with(&http_client(), paths, pending, region, progress, cancel).await
    }

    /// 台服實際回應（同 `patch::list` 測試素材）：game＋ex1 共 9 筆。
    pub(crate) const TW_BODY: &[u8] = include_bytes!("testdata/tw_patchlist_2026-10-13.bin");
    pub(crate) const TW_CONTENT_TYPE: &str = include_str!("testdata/tw_patchlist_2026-10-13.ct");
    /// 清單第 7 筆 ex1 2026.08.07 的實際 patch（52,848 bytes），repo 內唯一的真實 patch 檔。
    pub(crate) const TW_EX1_PATCH: &[u8] = include_bytes!("testdata/D2026.08.07.0000.0000.patch");
    pub(crate) const EX1_VERSION: &str = "2026.08.07.0000.0000";
    /// 實際清單中這個 patch 的 URL 路徑；stub 以同一路徑供檔。
    pub(crate) const EX1_URL_PATH: &str = "/game/ex1/be3c0f25/D2026.08.07.0000.0000.patch";
    /// 假安裝的起始版本：早於清單的任意值（測試不重送版本檢查請求，值本身不影響流程）。
    pub(crate) const OLD_VERSION: &str = "2026.07.27.0000.0000";
    pub(crate) const GAME_VER: &str = r"game\ffxivgame.ver";
    pub(crate) const EX1_VER: &str = r"game\sqpack\ex1\ex1.ver";
    /// ex1 patch 唯一寫出的檔（見 zipatch 測試 `real_ex1_patch_applies_inside_game_root`）。
    pub(crate) const EX1_DAT: &str = r"game\sqpack\ex1\030100.win32.dat0";
    /// 假設定中的台服帳號。
    pub(crate) const ACCOUNT_ID: &str = "a1";
    /// ZiPatch 檔頭 magic（protocols §5.4.2）。
    const ZIPATCH_MAGIC: [u8; 12] = [
        0x91, b'Z', b'I', b'P', b'A', b'T', b'C', b'H', b'\r', b'\n', 0x1a, b'\n',
    ];

    /// 目錄下所有項目（相對路徑）與檔案內容，已排序：比較兩次快照可看出任何新增、刪除或改寫。
    pub(crate) type Snapshot = Vec<(PathBuf, Option<Vec<u8>>)>;

    pub(crate) fn snapshot(root: &Path) -> Snapshot {
        fn walk(base: &Path, dir: &Path, out: &mut Snapshot) {
            for entry in fs::read_dir(dir).expect("read_dir") {
                let path = entry.expect("entry").path();
                let rel = path.strip_prefix(base).expect("prefix").to_path_buf();
                if path.is_dir() {
                    out.push((rel, None));
                    walk(base, &path, out);
                } else {
                    out.push((rel, Some(fs::read(&path).expect("read"))));
                }
            }
        }
        let mut out = Snapshot::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    /// 台服的隔離可寫假安裝：`<root>\appdata` 為 [`AppPaths`]，`<root>\FFXIV TC` 為安裝根目錄
    /// （`game\ffxiv_dx11.exe`、`ffxivgame.ver`、`sqpack\ex1\ex1.ver`、空的 `boot\`），設定中只有台服路徑與
    /// 帳號 [`ACCOUNT_ID`]。不碰真實的 `%APPDATA%` 與遊戲安裝。[`create_for`](Self::create_for) 以同樣結構
    /// 建立另一區服的假安裝（國際服在可修改安裝內套用的測試）。
    pub(crate) struct TwInstall {
        pub(crate) paths: AppPaths,
        pub(crate) install: PathBuf,
    }

    impl TwInstall {
        pub(crate) fn create(root: &Path) -> Self {
            Self::create_for(root, Region::Tw)
        }

        pub(crate) fn create_for(root: &Path, region: Region) -> Self {
            let fx = Self::open(root);
            for (rel, content) in [
                (r"game\ffxiv_dx11.exe", "exe"),
                (GAME_VER, OLD_VERSION),
                (EX1_VER, OLD_VERSION),
            ] {
                let path = fx.path(rel);
                fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
                fs::write(path, content).expect("write");
            }
            fs::create_dir_all(fx.path("boot")).expect("mkdir boot");
            let mut config = Config::default();
            config.games.set(region, Some(fx.install.clone()));
            config.accounts.push(Account {
                id: ACCOUNT_ID.into(),
                name: "測試帳號".into(),
                region,
                username: "user@example.com".into(),
                otp: OtpMode::None,
                profile_id: None,
                language: Default::default(),
                remember_password: false,
                extra_args: String::new(),
                act_follow: true,
            });
            store::save_config(&fx.paths, &config).expect("config");
            fx
        }

        /// 已由 [`create`](Self::create) 在 `root` 建好的同一份假安裝（子程序用）。
        pub(crate) fn open(root: &Path) -> Self {
            Self {
                paths: AppPaths::new(root.join("appdata")),
                install: root.join("FFXIV TC"),
            }
        }

        pub(crate) fn path(&self, rel: &str) -> PathBuf {
            self.install.join(rel)
        }

        pub(crate) fn read(&self, rel: &str) -> String {
            fs::read_to_string(self.path(rel)).expect("read")
        }

        pub(crate) fn snapshot(&self) -> Snapshot {
            snapshot(&self.install)
        }

        /// `patches\` 下殘留的檔案（下載的 patch、job）。
        pub(crate) fn downloaded_files(&self) -> Vec<PathBuf> {
            let dir = self.paths.patches_dir();
            if !dir.exists() {
                return Vec::new();
            }
            snapshot(&dir)
                .into_iter()
                .filter_map(|(rel, content)| content.map(|_| rel))
                .collect()
        }

        /// 以 `patches` 為這個安裝的台服 pending。
        pub(crate) fn pending(&self, patches: Vec<PatchEntry>) -> PendingUpdates {
            self.pending_for(Region::Tw, patches)
        }

        pub(crate) fn pending_for(&self, region: Region, patches: Vec<PatchEntry>) -> PendingUpdates {
            let pending = PendingUpdates::default();
            pending.set(
                region,
                Some(PendingUpdate {
                    install: self.install.clone(),
                    patches,
                }),
            );
            pending
        }
    }

    /// 實際台服回應解析出的 ex1 2026.08.07，URL 換成 stub 上的同一路徑。
    pub(crate) fn real_ex1_entry(stub: &Stub) -> PatchEntry {
        let mut entry = list::parse_patch_list(Region::Tw, TW_CONTENT_TYPE.trim(), TW_BODY)
            .expect("parse")
            .into_iter()
            .find(|e| e.repo == Repo::Expansion(1) && e.version_id == EX1_VERSION)
            .expect("ex1 2026.08.07");
        assert_eq!(entry.url.path(), EX1_URL_PATH);
        entry.url = stub.url(EX1_URL_PATH);
        entry
    }

    /// 測試自造的 patch 條目：長度與區塊 SHA1 由 `bytes` 算出（單一區塊），stub 以 `path` 供檔。
    pub(crate) fn made_entry(
        stub: &Stub,
        path: &str,
        repo: Repo,
        version_id: &str,
        bytes: &[u8],
    ) -> PatchEntry {
        PatchEntry {
            length: bytes.len() as u64,
            version_id: version_id.into(),
            hash_block_size: 50_000_000,
            hashes: vec![hex::encode(Sha1::digest(bytes))],
            url: stub.url(path),
            repo,
            file_name: path.rsplit('/').next().expect("file name").into(),
        }
    }

    /// 格式正確、不寫任何檔的 ZiPatch：檔頭＋`EOF_`（body 為 32 bytes 的 0，同實際樣本）。
    pub(crate) fn empty_zipatch() -> Vec<u8> {
        let body = [0u8; 32];
        let mut crc = crc32fast::Hasher::new();
        crc.update(b"EOF_");
        crc.update(&body);
        let mut out = ZIPATCH_MAGIC.to_vec();
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(b"EOF_");
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc.finalize().to_be_bytes());
        out
    }

    // ── 本機 stub server ─────────────────────────────────────────────────────

    /// 依請求路徑回 `(狀態行與標頭（不含結尾空行）, body)`；第二個參數是 stub 的 port。
    pub(crate) type Route = Arc<dyn Fn(&str, u16) -> (String, Vec<u8>) + Send + Sync>;

    pub(crate) struct Stub {
        base: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl Stub {
        pub(crate) fn requests(&self) -> Vec<String> {
            self.requests.lock().expect("lock").clone()
        }

        pub(crate) fn url(&self, path: &str) -> Url {
            Url::parse(&format!("{}{path}", self.base)).expect("url")
        }
    }

    pub(crate) fn spawn_stub(route: Route) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (route, log) = (Arc::clone(&route), Arc::clone(&log));
                std::thread::spawn(move || respond(stream, addr.port(), &route, &log));
            }
        });
        Stub {
            base: format!("http://{addr}"),
            requests,
        }
    }

    /// 依路徑供檔（`(URL 路徑, bytes)`），其他路徑 404。
    pub(crate) fn serve_files(files: Vec<(&str, Vec<u8>)>) -> Stub {
        let files: Vec<(String, Vec<u8>)> = files
            .into_iter()
            .map(|(path, body)| (path.to_string(), body))
            .collect();
        spawn_stub(Arc::new(move |path: &str, _: u16| {
            files
                .iter()
                .find(|(served, _)| served == path)
                .map_or_else(not_found, |(_, body)| ok(body))
        }))
    }

    fn respond(
        mut stream: TcpStream,
        port: u16,
        route: &Route,
        log: &Mutex<Vec<String>>,
    ) {
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            if line.trim_end().is_empty() {
                break;
            }
        }
        let path = request_line.split_whitespace().nth(1).unwrap_or("").to_string();
        log.lock().expect("lock").push(path.clone());
        let (head, body) = route(&path, port);
        // 用戶端只取標頭就斷線時寫入失敗，直接結束。
        if stream
            .write_all(format!("{head}\r\nConnection: close\r\n\r\n").as_bytes())
            .is_ok()
        {
            let _ = stream.write_all(&body);
        }
    }

    pub(crate) fn ok(body: &[u8]) -> (String, Vec<u8>) {
        (
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}", body.len()),
            body.to_vec(),
        )
    }

    pub(crate) fn redirect(location: String) -> (String, Vec<u8>) {
        (
            format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0"),
            Vec::new(),
        )
    }

    pub(crate) fn not_found() -> (String, Vec<u8>) {
        ("HTTP/1.1 404 Not Found\r\nContent-Length: 0".into(), Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::test_support::{
        EX1_DAT, EX1_URL_PATH, EX1_VER, EX1_VERSION, GAME_VER, OLD_VERSION, Snapshot, Stub,
        TW_BODY, TW_CONTENT_TYPE, TW_EX1_PATCH, TwInstall, empty_zipatch, http_client, made_entry,
        not_found, ok, real_ex1_entry, redirect, serve_files, spawn_stub, update_over_http,
    };
    use super::*;
    use crate::dalamud::DownloadCancel;
    use crate::patch::running::test_support::{FakeProcess, GAME_PROCESSES};

    fn tw_list() -> RawPatchList {
        RawPatchList {
            content_type: TW_CONTENT_TYPE.trim().to_string(),
            body: TW_BODY.to_vec(),
        }
    }

    #[test]
    fn patch_list_becomes_pending_and_update_required() {
        let pending = PendingUpdates::default();
        let install = Path::new(r"C:\Games\FFXIV TC");
        let err = require_up_to_date(
            &pending,
            Region::Tw,
            install,
            VersionCheck::<()>::PatchList(tw_list()),
        )
        .unwrap_err();
        let stored = pending.get(Region::Tw).expect("pending");
        assert_eq!(stored.install, install);
        assert_eq!(stored.patches.len(), 9);
        let total = total_length(&stored.patches);
        assert!(matches!(
            err,
            AppError::UpdateRequired { region: Region::Tw, patch_count: 9, total_bytes } if total_bytes == total
        ));
        assert!(pending.get(Region::Global).is_none(), "另一區服不受影響");

        // 之後的版本檢查無需更新：pending 清掉。
        let value = require_up_to_date(&pending, Region::Tw, install, VersionCheck::UpToDate(7))
            .expect("up to date");
        assert_eq!(value, 7);
        assert!(pending.get(Region::Tw).is_none());
    }

    #[test]
    fn rejected_list_clears_pending_and_is_returned() {
        let pending = PendingUpdates::default();
        let install = Path::new(r"C:\Games\FFXIV TC");
        store_patch_list(&pending, Region::Tw, install, &tw_list()).expect("store");

        let mut broken = tw_list();
        broken.content_type = "text/plain".into();
        assert!(matches!(
            require_up_to_date(&pending, Region::Tw, install, VersionCheck::<()>::PatchList(broken)),
            Err(AppError::PatchListRejected { region: Region::Tw, .. })
        ));
        assert!(pending.get(Region::Tw).is_none());

        // 國際服 patch host 尚未確認（protocols §5.1），清單一律拒絕。
        assert!(matches!(
            store_patch_list(&pending, Region::Global, install, &tw_list()),
            Err(AppError::PatchListRejected { region: Region::Global, .. })
        ));
        assert!(pending.get(Region::Global).is_none());
    }

    #[test]
    fn patch_state_serializes_only_summary_without_secrets() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().join("appdata"));
        let pending = PendingUpdates::default();
        store_patch_list(&pending, Region::Tw, dir.path(), &tw_list()).expect("store");

        let json = serde_json::to_value(patch_state(&paths, &pending).expect("state")).expect("json");
        let tw = &json["tw"];
        assert_eq!(tw["dirty"], false);
        let summary = tw["pending"].as_object().expect("pending summary");
        let mut keys: Vec<&str> = summary.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["patchCount", "totalBytes"]);
        assert_eq!(json["global"]["pending"], serde_json::Value::Null);
        let text = json.to_string().to_lowercase();
        for secret_field in ["uid", "uniqueid", "sid", "session"] {
            assert!(!text.contains(secret_field), "{secret_field}");
        }
        // pending 本身也只有清單與安裝路徑（RawPatchList 的 Debug 不印 body）。
        let raw = format!("{:?}", tw_list());
        assert!(!raw.contains("patch-dl"), "{raw}");
    }

    #[test]
    fn status_and_progress_shapes_match_frontend_contract() {
        let status = GameUpdateStatus::UpdateAvailable(PendingSummary {
            patch_count: 2,
            total_bytes: 10,
        });
        assert_eq!(
            serde_json::to_value(status).expect("json"),
            serde_json::json!({"kind": "updateAvailable", "patchCount": 2, "totalBytes": 10})
        );
        assert_eq!(
            serde_json::to_value(GameUpdateStatus::LoginRequired).expect("json"),
            serde_json::json!({"kind": "loginRequired"})
        );
        let progress = GameUpdateProgress::Download {
            patch_index: 1,
            patch_count: 3,
            done: 5,
            total: 9,
        };
        assert_eq!(
            serde_json::to_value(progress).expect("json"),
            serde_json::json!({"kind": "download", "patchIndex": 1, "patchCount": 3, "done": 5, "total": 9})
        );
    }

    #[test]
    fn missing_or_empty_version_file_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let install = dir.path();
        assert!(matches!(
            required_game_version(Region::Tw, install),
            Err(AppError::VersionFileMissing { region: Region::Tw, .. })
        ));
        let game = install.join("game");
        fs::create_dir_all(&game).expect("mkdir");
        fs::write(game.join("ffxivgame.ver"), " \r\n").expect("write");
        assert!(matches!(
            required_game_version(Region::Tw, install),
            Err(AppError::VersionFileMissing { .. })
        ));
        fs::write(game.join("ffxivgame.ver"), "2026.09.14.0000.0000\r\n").expect("write");
        assert_eq!(
            required_game_version(Region::Tw, install).expect("ver"),
            "2026.09.14.0000.0000"
        );
        assert!(matches!(
            required_expansion_version(Region::Global, install, 2),
            Err(AppError::VersionFileMissing { region: Region::Global, .. })
        ));
    }

    #[test]
    fn space_check_requires_total_on_every_volume() {
        let download = Path::new(r"C:\Users\x\AppData\Roaming\xiv-launch-manager\patches\tw");
        let install = Path::new(r"D:\Games\FFXIV");
        assert_eq!(insufficient_space(100, &[(download, 100), (install, 500)]), None);
        assert_eq!(
            insufficient_space(100, &[(download, 99), (install, 500)]),
            Some((download, 99))
        );
        assert_eq!(
            insufficient_space(100, &[(download, 500), (install, 0)]),
            Some((install, 0))
        );
        assert_eq!(insufficient_space(0, &[(download, 0), (install, 0)]), None);
    }

    // ── 下載（本機 stub server）─────────────────────────────────────────────

    async fn fetch(stub: &Stub, path: &str, dest: &Path, length: u64) -> AppResult<()> {
        fetch_with(http_client(), stub, path, dest, length).await
    }

    async fn fetch_with(
        client: reqwest::Client,
        stub: &Stub,
        path: &str,
        dest: &Path,
        length: u64,
    ) -> AppResult<()> {
        let cancel = DownloadCancel::default();
        download_patch(&client, &stub.url(path), dest, length, &|_| {}, &cancel.token()).await
    }

    const BODY: &[u8] = b"ZIPATCH sample body bytes";

    /// 清單長度傳到實際下載的回應：唯一一個請求的 `Content-Length` 不符即中止，不建立檔案。
    #[tokio::test(flavor = "multi_thread")]
    async fn list_length_mismatch_is_rejected_before_creating_file() {
        let stub = spawn_stub(Arc::new(|path: &str, _: u16| match path {
            "/a.patch" => ok(BODY),
            _ => not_found(),
        }));
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("game").join("a.patch");

        let err = fetch(&stub, "/a.patch", &dest, BODY.len() as u64 + 1)
            .await
            .unwrap_err();

        assert!(matches!(err, AppError::Protocol { .. }), "{err}");
        assert!(!dest.exists(), "長度不符時不建立檔案");
        assert_eq!(stub.requests(), ["/a.patch"], "不另送請求");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cross_host_redirect_is_rejected() {
        let stub = spawn_stub(Arc::new(|path: &str, port: u16| match path {
            // 同一台 stub，但 host 名稱不同（127.0.0.1 → localhost）。
            "/a.patch" => redirect(format!("http://localhost:{port}/b.patch")),
            "/b.patch" => ok(BODY),
            _ => not_found(),
        }));
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("a.patch");

        let err = fetch(&stub, "/a.patch", &dest, BODY.len() as u64)
            .await
            .unwrap_err();

        assert!(matches!(err, AppError::Network { .. }), "{err}");
        assert!(!dest.exists());
        assert_eq!(stub.requests(), ["/a.patch"], "不跟隨到另一個 host");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn same_host_redirect_is_followed_and_file_matches() {
        let stub = spawn_stub(Arc::new(|path: &str, _: u16| match path {
            "/a.patch" => redirect("/b.patch".into()),
            "/b.patch" => ok(BODY),
            _ => not_found(),
        }));
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("game").join("a.patch");

        fetch(&stub, "/a.patch", &dest, BODY.len() as u64)
            .await
            .expect("download");

        assert_eq!(fs::read(&dest).expect("read"), BODY);
        assert_eq!(stub.requests(), ["/a.patch", "/b.patch"]);
    }

    /// 台服下載只走 HTTPS：client 對 http 位址（正常解析不會出現，清單 URL 已改成 https）在送出前就拒絕，
    /// 不會以明文下載；國際服照清單的 http 下載（上面幾個測試）。
    #[tokio::test(flavor = "multi_thread")]
    async fn tw_transport_refuses_plain_http_before_sending() {
        assert_eq!(PatchTransport::of(Region::Tw), PatchTransport::HttpsOnly);
        assert_eq!(PatchTransport::of(Region::Global), PatchTransport::AsListed);
        let stub = spawn_stub(Arc::new(|path: &str, _: u16| match path {
            "/a.patch" => ok(BODY),
            _ => not_found(),
        }));
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("a.patch");
        let client = build_patch_client(
            reqwest::Client::builder().no_proxy(),
            PatchTransport::HttpsOnly,
        )
        .expect("client");

        let err = fetch_with(client, &stub, "/a.patch", &dest, BODY.len() as u64)
            .await
            .unwrap_err();

        assert!(matches!(err, AppError::Network { .. }), "{err}");
        assert!(!dest.exists());
        assert!(stub.requests().is_empty(), "沒有送出明文請求：{:?}", stub.requests());
    }

    /// 轉址規則：台服只跟隨同 host 的 https 轉址（降級成 http、換 host 都拒絕），國際服照舊只限同 host。
    /// 端對端的「https 轉址到 http」需要本機 TLS server，repo 沒有對應的測試依賴，這裡測規則本身（client 以
    /// [`redirect_rule`] 為轉址 policy，另有 reqwest `https_only` 擋 http 目標）。
    #[test]
    fn redirect_rule_keeps_host_and_never_downgrades_tw() {
        let url = |s: &str| Url::parse(s).expect("url");
        let tw = [url("https://patch-dl.ffxiv.com.tw/game/0b90d03e/a.patch")];
        let next = |s| redirect_rule(PatchTransport::HttpsOnly, &tw, &url(s));
        assert_eq!(next("https://patch-dl.ffxiv.com.tw/game/0b90d03e/b.patch"), Ok(()));
        assert_eq!(
            next("http://patch-dl.ffxiv.com.tw/game/0b90d03e/b.patch"),
            Err("patch 下載不跟隨降級成 http 的轉址")
        );
        assert_eq!(
            next("https://evil.example/game/0b90d03e/b.patch"),
            Err("patch 下載不跟隨跨 host 的轉址")
        );
        let hops = vec![tw[0].clone(); MAX_REDIRECTS + 1];
        assert_eq!(
            redirect_rule(PatchTransport::HttpsOnly, &hops, &tw[0]),
            Err("轉址次數過多")
        );

        let global = [url("http://patch-dl.ffxiv.com/game/0b90d03e/a.patch")];
        let next = |s| redirect_rule(PatchTransport::AsListed, &global, &url(s));
        assert_eq!(next("http://patch-dl.ffxiv.com/game/0b90d03e/b.patch"), Ok(()));
        assert_eq!(
            next("http://evil.example/b.patch"),
            Err("patch 下載不跟隨跨 host 的轉址")
        );
    }

    // ── 台服完整流程（update_with；本機 stub 供 patch、隔離可寫假安裝）────────────
    //
    // 台服正式下載只走 HTTPS，本機 stub 只有 http：流程測試經 `update_over_http` 換用接受 http 的 client
    // （其餘與 update_game 相同）。loopback 不經 proxy。

    /// 清單中 game 2026.08.17／08.24 的 URL 路徑；內容為測試自造（repo 沒有這兩個實際 patch）。
    const GAME_0817_URL_PATH: &str = "/game/0b90d03e/D2026.08.17.0000.0001.patch";
    const GAME_0824_URL_PATH: &str = "/game/0b90d03e/D2026.08.24.0000.0000.patch";

    async fn run_update(
        fx: &TwInstall,
        pending: &PendingUpdates,
        progress: &GameUpdateProgressFn,
    ) -> AppResult<()> {
        let cancel = DownloadCancel::default();
        update_over_http(&fx.paths, pending, Region::Tw, progress, &cancel.token()).await
    }

    /// 套用前就結束的失敗：下載的 patch 刪除、不留 dirty 標記、遊戲目錄不變、不提示修復。
    fn assert_clean_abort(fx: &TwInstall, before: &Snapshot, err: &AppError) {
        assert!(fx.downloaded_files().is_empty(), "{:?}", fx.downloaded_files());
        assert!(!dirty::is_dirty(&fx.paths, &fx.install).expect("dirty"));
        assert_eq!(&fx.snapshot(), before);
        assert!(!err.to_string().contains("修復"), "{err}");
    }

    /// 收集 update_game 的進度事件（進度回呼的型別要求 `'static`，以 `Arc` 共用）。
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<GameUpdateProgress>>>);

    impl Recorder {
        fn push(&self, p: GameUpdateProgress) {
            self.0.lock().expect("lock").push(p);
        }

        fn events(&self) -> Vec<GameUpdateProgress> {
            self.0.lock().expect("lock").clone()
        }
    }

    /// 進度事件的先後階段。
    fn stage(p: &GameUpdateProgress) -> u8 {
        match p {
            GameUpdateProgress::Download { .. } => 0,
            GameUpdateProgress::Verify { .. } => 1,
            GameUpdateProgress::Elevate => 2,
            GameUpdateProgress::Apply { .. } => 3,
        }
    }

    /// 驗收「台服：以真實回應＋隔離可寫 fixture 走完檢查→下載→核對→套用→重讀版本」。清單是實際台服回應；
    /// 版本檢查的 HTTP 端點寫死在 `tw::login`，這裡從回應交回之後開始（交回規則見
    /// `tw::login::tests::version_check_2xx_with_patch_url_hands_back_list`）。repo 只有 ex1 2026.08.07 的
    /// 實際 patch，pending 只留這一筆，URL 換成本機 stub 上的同一路徑。
    #[tokio::test(flavor = "multi_thread")]
    async fn tw_update_end_to_end_with_real_list_and_patch() {
        let _processes = GAME_PROCESSES.read().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let fx = TwInstall::create(dir.path());
        let pending = PendingUpdates::default();

        // 檢查：實際回應存成 pending，啟動擋在 UpdateRequired。
        let err = require_up_to_date(
            &pending,
            Region::Tw,
            &fx.install,
            VersionCheck::<()>::PatchList(tw_list()),
        )
        .unwrap_err();
        assert!(
            matches!(err, AppError::UpdateRequired { region: Region::Tw, patch_count: 9, .. }),
            "{err}"
        );
        let mut update = pending.get(Region::Tw).expect("pending");
        assert_eq!(update.install, fx.install);
        let stub = serve_files(vec![(EX1_URL_PATH, TW_EX1_PATCH.to_vec())]);
        update
            .patches
            .retain(|e| e.repo == Repo::Expansion(1) && e.version_id == EX1_VERSION);
        assert_eq!(update.patches.len(), 1);
        update.patches[0].url = stub.url(update.patches[0].url.path());
        pending.set(Region::Tw, Some(update));

        // 下載 → 核對 → 套用。
        let seen = Recorder::default();
        let record = seen.clone();
        run_update(&fx, &pending, &move |p| record.push(p))
            .await
            .expect("update");

        // 重讀版本：ex1 推進，game 未在本次清單內、不變。
        assert_eq!(
            required_expansion_version(Region::Tw, &fx.install, 1).expect("ex1.ver"),
            EX1_VERSION
        );
        assert_eq!(required_game_version(Region::Tw, &fx.install).expect("game ver"), OLD_VERSION);
        assert!(fx.path(EX1_DAT).is_file());
        assert!(!dirty::is_dirty(&fx.paths, &fx.install).expect("dirty"), "成功後清除標記");
        assert!(pending.get(Region::Tw).is_none());
        assert!(fx.downloaded_files().is_empty(), "套用成功的 patch 檔刪除");
        assert_eq!(stub.requests(), [EX1_URL_PATH], "只下載一次");
        let seen = seen.events();
        assert!(seen.iter().map(stage).is_sorted(), "{seen:?}");
        assert!(seen.iter().any(|p| stage(p) == 1), "{seen:?}");
        let total = TW_EX1_PATCH.len() as u64;
        assert!(
            matches!(
                seen.last(),
                Some(GameUpdateProgress::Apply { patch_index: 0, patch_count: 1, done, total: t })
                    if *done == total && *t == total
            ),
            "{seen:?}"
        );
    }

    /// 驗收「核對失敗：只清理 patch 檔，不留 dirty 標記、不提示修復」：伺服器給的 bytes 長度相同但內容不符。
    #[tokio::test(flavor = "multi_thread")]
    async fn tw_verify_failure_deletes_download_and_leaves_no_dirty() {
        let _processes = GAME_PROCESSES.read().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let fx = TwInstall::create(dir.path());
        let before = fx.snapshot();
        let mut tampered = TW_EX1_PATCH.to_vec();
        tampered[TW_EX1_PATCH.len() / 2] ^= 0xff;
        let stub = serve_files(vec![(EX1_URL_PATH, tampered)]);
        let pending = fx.pending(vec![real_ex1_entry(&stub)]);

        let err = run_update(&fx, &pending, &|_| {}).await.unwrap_err();

        assert!(matches!(err, AppError::PatchVerifyFailed { .. }), "{err}");
        assert_clean_abort(&fx, &before, &err);
        assert!(pending.get(Region::Tw).is_some(), "沒開始套用，pending 保留可重試");
    }

    /// 驗收「套用前取消：只清理 patch 檔，不留 dirty 標記、不提示修復」：下載並核對完、套用前取消。
    #[tokio::test(flavor = "multi_thread")]
    async fn tw_cancel_before_apply_deletes_download_and_leaves_no_dirty() {
        let _processes = GAME_PROCESSES.read().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let fx = TwInstall::create(dir.path());
        let before = fx.snapshot();
        let stub = serve_files(vec![(EX1_URL_PATH, TW_EX1_PATCH.to_vec())]);
        let pending = fx.pending(vec![real_ex1_entry(&stub)]);
        let cancel = Arc::new(DownloadCancel::default());
        let seen = Recorder::default();
        let (canceller, record) = (Arc::clone(&cancel), seen.clone());

        let err = update_over_http(
            &fx.paths,
            &pending,
            Region::Tw,
            &move |p| {
                if matches!(p, GameUpdateProgress::Verify { .. }) {
                    canceller.cancel();
                }
                record.push(p);
            },
            &cancel.token(),
        )
        .await
        .unwrap_err();

        assert!(matches!(err, AppError::Cancelled), "{err}");
        assert_clean_abort(&fx, &before, &err);
        assert_eq!(stub.requests(), [EX1_URL_PATH], "已下載完才取消");
        let seen = seen.events();
        assert!(!seen.iter().any(|p| stage(p) == 3), "沒有進入套用：{seen:?}");
    }

    /// 驗收「逐 patch 推進版本」（寫入前失敗）：ex1 實際 patch 成功、game 08.17 不是 ZiPatch（通過下載核對、
    /// 掃描拒絕）、game 08.24 未開始。ex1 的版本保留，game 不推進；失敗的 patch 未寫入任何檔，清除標記、不提示修復。
    #[tokio::test(flavor = "multi_thread")]
    async fn tw_later_patch_failing_before_write_keeps_earlier_version_and_clears_dirty() {
        let _processes = GAME_PROCESSES.read().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let fx = TwInstall::create(dir.path());
        let not_zipatch = b"not a zipatch ".repeat(16);
        let empty = empty_zipatch();
        let stub = serve_files(vec![
            (EX1_URL_PATH, TW_EX1_PATCH.to_vec()),
            (GAME_0817_URL_PATH, not_zipatch.clone()),
            (GAME_0824_URL_PATH, empty.clone()),
        ]);
        let pending = fx.pending(vec![
            real_ex1_entry(&stub),
            made_entry(&stub, GAME_0817_URL_PATH, Repo::Game, "2026.08.17.0000.0001", &not_zipatch),
            made_entry(&stub, GAME_0824_URL_PATH, Repo::Game, "2026.08.24.0000.0000", &empty),
        ]);

        let err = run_update(&fx, &pending, &|_| {}).await.unwrap_err();

        assert!(matches!(err, AppError::PatchRejected { .. }), "{err}");
        assert!(!err.to_string().contains("修復"), "{err}");
        assert_eq!(fx.read(EX1_VER), EX1_VERSION, "前面成功的 patch 版本保留");
        assert_eq!(fx.read(GAME_VER), OLD_VERSION, "失敗（08.17）與未開始（08.24）的不推進");
        assert!(!dirty::is_dirty(&fx.paths, &fx.install).expect("dirty"));
        assert!(fx.downloaded_files().is_empty(), "{:?}", fx.downloaded_files());
        assert!(pending.get(Region::Tw).is_none(), "已開始套用，pending 作廢");
    }

    /// 驗收「逐 patch 推進版本」與「已開始套用後失敗才提示官方修復並保留標記」：ex1 實際 patch 成功；game
    /// 08.17（格式正確的空 patch）套用後寫 `ffxivgame.ver` 失敗（位置被非空目錄佔住）；ex2 08.24 未開始。
    #[tokio::test(flavor = "multi_thread")]
    async fn tw_later_patch_failing_after_write_keeps_earlier_version_and_dirty() {
        let _processes = GAME_PROCESSES.read().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let fx = TwInstall::create(dir.path());
        fs::remove_file(fx.path(GAME_VER)).expect("rm ver");
        fs::create_dir(fx.path(GAME_VER)).expect("mkdir");
        fs::write(fx.path(GAME_VER).join("keep"), b"").expect("seed");
        fs::create_dir_all(fx.path(r"game\sqpack\ex2")).expect("mkdir ex2");
        let empty = empty_zipatch();
        let ex2_path = "/game/ex2/00000000/D2026.08.24.0000.0000.patch";
        let stub = serve_files(vec![
            (EX1_URL_PATH, TW_EX1_PATCH.to_vec()),
            (GAME_0817_URL_PATH, empty.clone()),
            (ex2_path, empty.clone()),
        ]);
        let pending = fx.pending(vec![
            real_ex1_entry(&stub),
            made_entry(&stub, GAME_0817_URL_PATH, Repo::Game, "2026.08.17.0000.0001", &empty),
            made_entry(&stub, ex2_path, Repo::Expansion(2), "2026.08.24.0000.0000", &empty),
        ]);

        let err = run_update(&fx, &pending, &|_| {}).await.unwrap_err();

        assert!(matches!(err, AppError::UpdateBroken { region: Region::Tw, .. }), "{err}");
        assert!(err.to_string().contains("官方啟動器修復"), "{err}");
        assert!(dirty::is_dirty(&fx.paths, &fx.install).expect("dirty"), "保留標記");
        assert_eq!(fx.read(EX1_VER), EX1_VERSION, "前面成功的 patch 版本保留");
        assert!(!fx.path(r"game\sqpack\ex2\ex2.ver").exists(), "未開始的不推進");
        assert!(fx.downloaded_files().is_empty(), "{:?}", fx.downloaded_files());
        assert!(pending.get(Region::Tw).is_none());

        // 標記存在：再次更新在下載前就被拒。
        let pending = fx.pending(vec![real_ex1_entry(&stub)]);
        let requests = stub.requests().len();
        let err = run_update(&fx, &pending, &|_| {}).await.unwrap_err();
        assert!(matches!(err, AppError::PatchDirty { region: Region::Tw }), "{err}");
        assert_eq!(stub.requests().len(), requests);
    }

    /// 驗收「官方啟動器或外部啟動的遊戲運行中，更新在寫入前被拒」（開始更新前的檢查）：官方啟動器
    /// `boot\FfxivLauncherTC.exe` 以完整路徑執行中，下載之前就拒絕。
    #[tokio::test(flavor = "multi_thread")]
    async fn tw_update_refused_while_official_launcher_runs() {
        let _processes = GAME_PROCESSES.write().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let fx = TwInstall::create(dir.path());
        let _launcher = FakeProcess::start(&fx.path(r"boot\FfxivLauncherTC.exe"));
        let before = fx.snapshot();
        let stub = serve_files(vec![(EX1_URL_PATH, TW_EX1_PATCH.to_vec())]);
        let pending = fx.pending(vec![real_ex1_entry(&stub)]);

        let err = run_update(&fx, &pending, &|_| {}).await.unwrap_err();

        assert!(matches!(err, AppError::GameRunning), "{err}");
        assert!(stub.requests().is_empty(), "下載前就拒絕：{:?}", stub.requests());
        assert_clean_abort(&fx, &before, &err);
        assert!(pending.get(Region::Tw).is_some());
    }

    /// 驗收同上（台服程序內套用在第一筆寫入前再檢查）：下載完、核對時遊戲才以完整路徑啟動，套用在寫入前
    /// 拒絕；遊戲目錄未被寫過，清除標記。
    #[tokio::test(flavor = "multi_thread")]
    async fn tw_game_started_after_download_is_refused_before_first_write() {
        let _processes = GAME_PROCESSES.write().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let fx = TwInstall::create(dir.path());
        let game: Arc<Mutex<Option<FakeProcess>>> = Arc::default();
        let exe = fx.path(r"game\ffxiv_dx11.exe");
        let stub = serve_files(vec![(EX1_URL_PATH, TW_EX1_PATCH.to_vec())]);
        let pending = fx.pending(vec![real_ex1_entry(&stub)]);
        let seen = Recorder::default();
        let (starter, record) = (Arc::clone(&game), seen.clone());

        let err = run_update(&fx, &pending, &move |p| {
            if matches!(p, GameUpdateProgress::Verify { .. }) {
                starter
                    .lock()
                    .expect("lock")
                    .get_or_insert_with(|| FakeProcess::start(&exe));
            }
            record.push(p);
        })
        .await
        .unwrap_err();

        assert!(matches!(err, AppError::GameRunning), "{err}");
        assert!(game.lock().expect("lock").is_some(), "遊戲替身已啟動");
        assert_eq!(fx.read(EX1_VER), OLD_VERSION);
        assert!(!fx.path(EX1_DAT).exists(), "沒有寫入");
        let seen = seen.events();
        assert!(!seen.iter().any(|p| stage(p) == 3), "沒有進入套用：{seen:?}");
        assert!(!dirty::is_dirty(&fx.paths, &fx.install).expect("dirty"));
        assert!(fx.downloaded_files().is_empty(), "{:?}", fx.downloaded_files());
        assert!(!err.to_string().contains("修復"), "{err}");
    }

    /// 驗收「畸形清單（空 body、非 9 欄、hash 個數不符、檔名含 `..`、跨 host URL）皆被拒，不寫 `.ver`」：
    /// 由實際回應改出各類畸形（各條拒絕原因見 `patch::list` 的 `rejects_*` 測試），版本檢查交回後被拒、
    /// 原有 pending 清掉，之後的更新無清單可套用，遊戲目錄不變。
    #[tokio::test(flavor = "multi_thread")]
    async fn malformed_lists_are_rejected_and_nothing_is_written() {
        let text = std::str::from_utf8(TW_BODY).expect("utf8");
        let ex1 = "be3c0f25/D2026.08.07.0000.0000.patch";
        let cases = [
            ("空 body", String::new()),
            (
                "非 9 欄",
                text.replacen("\tsha1\t50000000\t625499a5", "\tsha1\t625499a5", 1),
            ),
            (
                "hash 個數不符",
                text.replacen(",92763a37e0486263f1d39d87da878aef8402ee9a", "", 1),
            ),
            ("檔名含 ..", text.replacen(ex1, "be3c0f25/D2026.08.07..patch", 1)),
            (
                "跨 host URL",
                text.replacen(
                    &format!("http://patch-dl.ffxiv.com.tw/game/ex1/{ex1}"),
                    &format!("http://evil.example/game/ex1/{ex1}"),
                    1,
                ),
            ),
        ];
        for (name, body) in cases {
            assert_ne!(body.as_bytes(), TW_BODY, "{name}");
            let dir = tempfile::tempdir().expect("tempdir");
            let fx = TwInstall::create(dir.path());
            let before = fx.snapshot();
            let pending = PendingUpdates::default();
            store_patch_list(&pending, Region::Tw, &fx.install, &tw_list()).expect("store");
            let raw = RawPatchList {
                content_type: TW_CONTENT_TYPE.trim().into(),
                body: body.into_bytes(),
            };

            let err = require_up_to_date(&pending, Region::Tw, &fx.install, VersionCheck::<()>::PatchList(raw))
                .unwrap_err();
            assert!(
                matches!(err, AppError::PatchListRejected { region: Region::Tw, .. }),
                "{name}: {err}"
            );
            assert!(pending.get(Region::Tw).is_none(), "{name}");

            let err = run_update(&fx, &pending, &|_| {}).await.unwrap_err();
            assert!(matches!(err, AppError::NotFound { .. }), "{name}: {err}");
            assert_eq!(fx.snapshot(), before, "{name}");
            assert!(!dirty::is_dirty(&fx.paths, &fx.install).expect("dirty"), "{name}");
            assert!(fx.downloaded_files().is_empty(), "{name}");
        }
    }

    // ── 國際服：套用方式分流（程序內／helper）與 helper 的結果判定（不啟動 helper、不經 UAC）──────────

    /// 套用方式的分流：台服一律程序內套用、不判定權限；國際服安裝目前程序可修改就程序內套用（不跳 UAC），
    /// 不可修改才經 helper；判定失敗不是「受保護」，回錯誤、不改走提權。
    #[test]
    fn apply_route_elevates_only_protected_global_install() {
        let tw = apply_route(Region::Tw, || -> AppResult<bool> { panic!("台服不判定權限") });
        assert_eq!(tw.expect("tw"), ApplyRoute::InProcess);
        assert_eq!(
            apply_route(Region::Global, || Ok(true)).expect("modifiable"),
            ApplyRoute::InProcess
        );
        assert_eq!(
            apply_route(Region::Global, || Ok(false)).expect("protected"),
            ApplyRoute::Elevated
        );
        let failed = apply_route(Region::Global, || {
            Err(AppError::Io {
                context: "讀取權限設定".into(),
                message: "denied".into(),
            })
        });
        assert!(matches!(failed, Err(AppError::Io { .. })), "{failed:?}");
    }

    /// 國際服安裝在目前程序可修改的位置（tempdir 假安裝）：走程序內套用，不送 Elevate、不啟動 helper，ex1 實際
    /// patch 套用並推進 `ex1.ver`。先確認判定確實是可修改，判定不符就在呼叫前失敗，不會走到 UAC。
    #[tokio::test(flavor = "multi_thread")]
    async fn global_modifiable_install_applies_in_process_without_uac() {
        let _processes = GAME_PROCESSES.read().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let fx = TwInstall::create_for(dir.path(), Region::Global);
        let token = AccessToken::current().expect("token");
        assert!(access::may_modify(&token, &fx.install).expect("check"), "假安裝須可修改");
        let stub = serve_files(vec![(EX1_URL_PATH, TW_EX1_PATCH.to_vec())]);
        let pending = fx.pending_for(Region::Global, vec![real_ex1_entry(&stub)]);
        let seen = Recorder::default();
        let record = seen.clone();
        let cancel = DownloadCancel::default();

        update_over_http(
            &fx.paths,
            &pending,
            Region::Global,
            &move |p| record.push(p),
            &cancel.token(),
        )
        .await
        .expect("update");

        assert_eq!(fx.read(EX1_VER), EX1_VERSION);
        assert!(fx.path(EX1_DAT).is_file());
        assert!(!dirty::is_dirty(&fx.paths, &fx.install).expect("dirty"));
        assert!(pending.get(Region::Global).is_none());
        assert!(fx.downloaded_files().is_empty(), "{:?}", fx.downloaded_files());
        let seen = seen.events();
        assert!(!seen.iter().any(|p| matches!(p, GameUpdateProgress::Elevate)), "{seen:?}");
        assert!(seen.iter().any(|p| stage(p) == 3), "{seen:?}");
    }

    /// 主程式只以 helper 的 exit code 判定成敗（主 note「成敗與進度」）。驗收「UAC 拒絕：不留 dirty 標記、
    /// 不提示修復」的判定部分：helper 確定沒寫過遊戲目錄（寫入前中止、安裝一般權限可修改、UAC 拒絕、沒能
    /// 啟動）才清除標記，其他（互斥被佔、寫入後失敗、其他 exit code 含被結束與崩潰）保留並提示官方修復。
    #[test]
    fn helper_exit_classification_matches_dirty_policy() {
        assert!(matches!(classify_helper_exit(HelperExit::Success), Applied::Done));
        for exit in [
            HelperExit::AbortedBeforeWrite,
            HelperExit::InstallModifiable,
            HelperExit::UacDeclined,
            HelperExit::NotStarted,
        ] {
            match classify_helper_exit(exit) {
                Applied::Clean(error @ AppError::UpdateAborted { .. }) => {
                    assert!(!error.to_string().contains("修復"), "{exit:?}: {error}");
                }
                _ => panic!("{exit:?} 應清除標記"),
            }
        }
        for exit in [
            HelperExit::Busy,
            HelperExit::FailedAfterWrite,
            HelperExit::Other(2),
            HelperExit::Other(1),
            HelperExit::Other(-1_073_741_819),
        ] {
            match classify_helper_exit(exit) {
                Applied::Broken {
                    error: error @ AppError::UpdateBroken { region: Region::Global, .. },
                    helper_unknown: false,
                } => assert!(error.to_string().contains("官方啟動器修復"), "{exit:?}: {error}"),
                _ => panic!("{exit:?} 應保留標記"),
            }
        }
    }

    /// 驗收「預寫成功結果被拒」的 parent 端：job 目錄的進度檔與 job.json 都以 `create_new` 建立，預先放好的
    /// 「已完成」進度或自造 job 不會被沿用或覆寫（helper 端只接受空進度檔，見 `patch::helper` 測試）。
    #[test]
    fn prepared_job_refuses_prewritten_progress_or_job() {
        let dir = tempfile::tempdir().expect("tempdir");
        let install = dir.path().join("FINAL FANTASY XIV - A Realm Reborn");
        let patch = JobPatch {
            path: dir.path().join("D2026.08.07.0000.0000.patch"),
            repo: Repo::Expansion(1),
            version_id: EX1_VERSION.into(),
            length: TW_EX1_PATCH.len() as u64,
            hash_block_size: 50_000_000,
            hashes: vec!["625499a52faf29e527543ee969e17ff9a81edcb6".into()],
        };

        // 正常：job 目錄下的 progress.json 空檔＋job.json，helper 端以命令列的 SHA-256 讀回同一份。
        let fresh = dir.path().join("job-fresh");
        let (job_path, sha256) =
            prepare_job(&fresh, &install, vec![patch.clone()]).expect("prepare");
        assert_eq!(job_path.parent(), Some(fresh.as_path()));
        assert_eq!(fs::read(fresh.join("progress.json")).expect("progress"), b"");
        assert_eq!(job::read_job(&job_path, &sha256).expect("read").0.patches, vec![patch.clone()]);

        // 預寫「已完成」進度：拒絕，不覆寫、不寫 job。
        let forged = dir.path().join("job-forged-progress");
        let progress = job::progress_path(&forged);
        fs::create_dir_all(&forged).expect("mkdir");
        let done = br#"{"patchIndex":0,"patchCount":1,"done":52848,"total":52848}"#;
        fs::write(&progress, done).expect("seed");
        assert!(prepare_job(&forged, &install, vec![patch.clone()]).is_err());
        assert_eq!(fs::read(&progress).expect("progress"), done);
        assert!(!forged.join("job.json").exists());

        // 預寫 job.json：拒絕，不採用也不覆寫。
        let planted = dir.path().join("job-planted");
        fs::create_dir_all(&planted).expect("mkdir");
        fs::write(planted.join("job.json"), b"{}").expect("seed");
        assert!(prepare_job(&planted, &install, vec![patch]).is_err());
        assert_eq!(fs::read(planted.join("job.json")).expect("job"), b"{}");
    }
}
