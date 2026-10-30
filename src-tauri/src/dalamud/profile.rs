//! 衛月 profile 目錄與佔用紀錄（protocols §3）。
//!
//! 佔用紀錄 `state/profiles_running.json`：profile_id → `[{pid, created}, …]`（共用時多筆，protocols §3）；
//! `state/accounts_running.json`：account_id → `{pid, created}`（帶不帶衛月都記）。pid 會被系統重用，所以判斷
//! 「仍在執行」時 pid 與程序建立時間都要相符；任何查找都只用本程式取得的 pid，不按程序名。寫入時順便清掉已結束者。
//! 讀改寫在本模組的鎖內完成：登入在 app-wide mutex 內記錄，背景觀察到遊戲提早結束時在 mutex 外撤銷
//! （[`unmark_running`]、[`unmark_account_running`]），兩者不會互相蓋掉。

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_PARAMETER, FILETIME, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows::Win32::System::Threading::{
    GetProcessTimes, INFINITE, OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
};

use super::install::{self, CancelToken, Progress, ProgressFn, Staging};
use crate::error::{AppError, AppResult};
use crate::store::{self, AppPaths, Profile};

/// 一個 profile 交給 Injector 的路徑。
#[derive(Debug, Clone)]
pub struct ProfilePaths {
    pub config_file: PathBuf,
    pub plugin_dir: PathBuf,
    pub dev_plugin_dir: PathBuf,
    pub log_dir: PathBuf,
}

/// profile 的衛月資料根目錄：沿用外部資料夾時為 `data_dir`，否則本程式的 `profiles/<id>/`。
pub fn profile_data_dir(paths: &AppPaths, profile: &Profile) -> AppResult<PathBuf> {
    check_profile_id(&profile.id)?;
    Ok(match &profile.data_dir {
        Some(dir) => dir.clone(),
        None => paths.profile_dir(&profile.id),
    })
}

/// log 一律寫本程式的 `profiles/<id>/logs`，不在別人的資料夾留檔。
pub fn profile_log_dir(paths: &AppPaths, profile: &Profile) -> AppResult<PathBuf> {
    check_profile_id(&profile.id)?;
    Ok(paths.profile_dir(&profile.id).join("logs"))
}

/// id 會變成目錄名稱，不可含路徑分隔或 `..`。
fn check_profile_id(id: &str) -> AppResult<()> {
    if install::is_safe_component(id) {
        Ok(())
    } else {
        Err(AppError::NotFound {
            what: format!("Dalamud 設定檔「{id}」"),
        })
    }
}

/// 決定 profile 交給 Injector 的路徑。`data_dir` 為 `None` 時用 `profiles/<id>/`
/// （`dalamudConfig.json` 由衛月自己建立）；沿用外部資料夾時 config 必須已存在，只補建缺少的
/// 插件目錄。
pub fn ensure_profile_paths(paths: &AppPaths, profile: &Profile) -> AppResult<ProfilePaths> {
    let data = profile_data_dir(paths, profile)?;
    if profile.data_dir.is_some() && !data.join("dalamudConfig.json").is_file() {
        return Err(AppError::NotFound {
            what: "Dalamud 資料夾".into(),
        });
    }
    let result = ProfilePaths {
        config_file: data.join("dalamudConfig.json"),
        plugin_dir: data.join("installedPlugins"),
        dev_plugin_dir: data.join("devPlugins"),
        log_dir: profile_log_dir(paths, profile)?,
    };
    for d in [&result.plugin_dir, &result.dev_plugin_dir, &result.log_dir] {
        fs::create_dir_all(d).map_err(|e| AppError::io("建立 profile 目錄", e))?;
    }
    Ok(result)
}

/// 複製設定檔時帶走什麼：設定＋插件本體，不帶插件資料與快取。
#[derive(Clone, Copy)]
enum Scope {
    /// 整個檔案或資料夾樹。
    Tree,
    /// 資料夾本身與其最上層的一般檔案（`pluginConfigs\` 的各插件設定 `*.json` 等）；子資料夾是插件資料與
    /// 快取（本機 ShaderProbe 81 GB、AnoMech 17 GB），不複製，插件首次執行時會重建。
    TopFiles,
    /// [`Scope::TopFiles`] 底下的一項：只收一般檔案。
    FileOnly,
}

/// 複製設定檔時帶走的項目（相對資料根目錄）。依據：衛月經本程式啟動後在 `profiles\<id>\` 實際寫入的是
/// 這些加上 `logs\`、`dalamudVfs.db`（2026-10-14 本機實測）；`dalamudUI.ini`、`pluginConfigs\` 跟著
/// `--dalamud-configuration-path` 所在目錄（protocols §4）。不複製：`logs\`；`dalamudVfs.db`（衛月
/// ReliableFileStorage 的設定備份，以原檔絕對路徑為 key，副本的路徑對不上、等於無用，衛月找不到會重建）。
/// 沿用的舊啟動器資料夾另有啟動器自己的檔案（XIVLauncher 的 `addon\`、`runtime\`、`accountsList.json`、
/// log 等），不屬於設定檔，不帶走。
const DUPLICATED_ENTRIES: [(&str, Scope); 5] = [
    ("dalamudConfig.json", Scope::Tree),
    ("dalamudUI.ini", Scope::Tree),
    ("installedPlugins", Scope::Tree),
    ("devPlugins", Scope::Tree),
    ("pluginConfigs", Scope::TopFiles),
];
const DUPLICATE_STAGE: &str = "複製設定檔";
const COPY_BUF_SIZE: usize = 1024 * 1024;
/// 進度回報的最小間隔（bytes）。`installedPlugins\` 可達數百 MB，間隔太小會讓前端事件過多。
const DUPLICATE_PROGRESS_STEP: u64 = 8 * 1024 * 1024;

/// 一次複製的結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DuplicateReport {
    /// 略過的 reparse point（symlink、junction 等）數。
    pub skipped_links: u64,
}

/// 依序要建立的資料夾與要複製的檔案（相對資料根目錄；資料夾排在其內容之前）。
enum CopyItem {
    Dir(PathBuf),
    File(PathBuf),
}

struct CopyPlan {
    items: Vec<CopyItem>,
    total: u64,
    skipped_links: u64,
}

/// reparse point（symlink、junction 等）不跟隨、連結本身也不複製：跟隨可能複製到資料根目錄以外的東西，
/// 複製連結則讓副本與原本共用同一份資料（不是獨立副本），建立 symlink 又需要特權。
fn is_reparse_point(meta: &fs::Metadata) -> bool {
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
}

/// 先列出全部項目與總大小（進度的分母）；不存在的項目略過。
fn plan_copy(source: &Path) -> AppResult<CopyPlan> {
    let mut plan = CopyPlan {
        items: Vec::new(),
        total: 0,
        skipped_links: 0,
    };
    let mut pending: Vec<(PathBuf, Scope)> = DUPLICATED_ENTRIES
        .iter()
        .rev()
        .map(|&(rel, scope)| (PathBuf::from(rel), scope))
        .collect();
    while let Some((rel, scope)) = pending.pop() {
        let path = source.join(&rel);
        let read_error = |e| AppError::io(format!("讀取 {}", path.display()), e);
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(read_error(e)),
        };
        if is_reparse_point(&meta) {
            plan.skipped_links += 1;
        } else if meta.is_dir() {
            let child = match scope {
                Scope::Tree => Scope::Tree,
                Scope::TopFiles => Scope::FileOnly,
                // 插件資料與快取。
                Scope::FileOnly => continue,
            };
            for entry in fs::read_dir(&path).map_err(read_error)? {
                pending.push((rel.join(entry.map_err(read_error)?.file_name()), child));
            }
            plan.items.push(CopyItem::Dir(rel));
        } else if meta.is_file() {
            plan.total += meta.len();
            plan.items.push(CopyItem::File(rel));
        }
    }
    Ok(plan)
}

/// 依 `plan` 建立資料夾、複製檔案到 `dest`；`advance` 收到每段寫入的 bytes。每段之間檢查取消。
fn copy_planned(
    source: &Path,
    dest: &Path,
    plan: &CopyPlan,
    cancel: &CancelToken,
    advance: &mut dyn FnMut(u64),
) -> AppResult<()> {
    let mut buf = vec![0u8; COPY_BUF_SIZE];
    for item in &plan.items {
        cancel.check()?;
        match item {
            CopyItem::Dir(rel) => fs::create_dir(dest.join(rel))
                .map_err(|e| AppError::io(format!("建立 {}", rel.display()), e))?,
            CopyItem::File(rel) => {
                let context = || format!("複製 {}", source.join(rel).display());
                let mut from = File::open(source.join(rel)).map_err(|e| AppError::io(context(), e))?;
                let mut to = File::create_new(dest.join(rel)).map_err(|e| AppError::io(context(), e))?;
                loop {
                    cancel.check()?;
                    let n = match from.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(AppError::io(context(), e)),
                    };
                    to.write_all(&buf[..n]).map_err(|e| AppError::io(context(), e))?;
                    advance(n as u64);
                }
            }
        }
    }
    Ok(())
}

/// 把 `source` 的衛月資料（[`DUPLICATED_ENTRIES`]，不含 log）複製成本程式的 `profiles\<new_id>\`，
/// 不論來源是否沿用外部資料夾。先寫進 `staging\` 的暫存目錄，全部完成才 rename 成正式目錄；失敗或取消時
/// 暫存整個刪除，不留半成品。進度以 bytes 計。只能在 app-wide mutex 內呼叫（`staging\` 的殘留清理
/// 與遊戲啟動都在鎖內，複製期間來源不會被本程式啟動的遊戲改寫）。
pub async fn duplicate_profile_data(
    paths: &AppPaths,
    source: &Profile,
    new_id: &str,
    progress: &ProgressFn,
    cancel: &CancelToken,
) -> AppResult<DuplicateReport> {
    let from = profile_data_dir(paths, source)?;
    check_profile_id(new_id)?;
    // 同 ensure_profile_paths：沿用的資料夾沒有 config 就是找不到；本程式的 profile 還沒用過時可以是空的。
    if source.data_dir.is_some() && !from.join("dalamudConfig.json").is_file() {
        return Err(AppError::NotFound {
            what: "Dalamud 資料夾".into(),
        });
    }
    let target = paths.profile_dir(new_id);
    if target.exists() {
        return Err(AppError::Io {
            context: "建立設定檔資料夾".into(),
            message: format!("{} 已存在", target.display()),
        });
    }
    let staging = Staging::create(paths, "profile")?;
    let (tx, mut rx) = watch::channel((0u64, 0u64));
    let token = cancel.clone();
    // staging 由 blocking 工作持有：工作結束（含失敗）才 drop 刪除暫存，不會與仍在寫入的複製競爭。
    let job = tokio::task::spawn_blocking(move || {
        let content = staging.path().join("content");
        fs::create_dir(&content).map_err(|e| AppError::io("建立暫存目錄", e))?;
        let plan = plan_copy(&from)?;
        let total = plan.total;
        tx.send_replace((0, total));
        let (mut done, mut reported) = (0u64, 0u64);
        copy_planned(&from, &content, &plan, &token, &mut |n| {
            done += n;
            if done == total || done - reported >= DUPLICATE_PROGRESS_STEP {
                reported = done;
                tx.send_replace((done, total));
            }
        })?;
        token.check()?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| AppError::io("建立 profiles 目錄", e))?;
        }
        fs::rename(&content, &target).map_err(|e| AppError::io("建立設定檔資料夾", e))?;
        Ok(DuplicateReport {
            skipped_links: plan.skipped_links,
        })
    });
    // 發送端隨 blocking 工作結束而 drop，迴圈在最後一筆進度送出後結束。
    while rx.changed().await.is_ok() {
        let (done, total) = *rx.borrow_and_update();
        progress(Progress {
            stage: DUPLICATE_STAGE.to_string(),
            done,
            total: Some(total),
        });
    }
    job.await.map_err(|e| AppError::Io {
        context: DUPLICATE_STAGE.to_string(),
        message: e.to_string(),
    })?
}

/// 複製完成、但設定寫不進去時撤回 `profiles\<id>\`：先整個移進 `staging\`（從 profiles 消失是一次 rename），
/// 再刪除。只能在 app-wide mutex 內呼叫。
pub fn discard_profile_dir(paths: &AppPaths, id: &str) {
    let target = paths.profile_dir(id);
    // quality-allow: 撤回是失敗路徑上的 best-effort；移不開時資料夾留著，不影響設定（設定沒有寫入）
    if let Ok(aside) = install::move_aside(paths, &target) {
        // quality-allow: 已移出 profiles，刪不掉只是暫存殘留，下次 purge_stale 再清
        let _ = fs::remove_dir_all(aside);
    }
}

/// 遊戲程序身分：pid＋建立時間（`GetProcessTimes` 的 creation FILETIME，100ns 單位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GameProcess {
    pub pid: u32,
    pub created: u64,
}

/// 程序 handle 的 RAII 包裝，確保每個開啟的 handle 都會關閉。
struct ProcessHandle(HANDLE);

impl ProcessHandle {
    fn open(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> windows::core::Result<Self> {
        // SAFETY: OpenProcess 只讀取值參數；成功時回傳的 handle 由本型別獨占並在 Drop 關閉。
        // quality-allow: Win32 FFI，OpenProcess 沒有安全包裝
        let handle = unsafe { OpenProcess(access, false, pid) }?;
        Ok(Self(handle))
    }

    fn creation_time(&self) -> windows::core::Result<u64> {
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: self.0 是含 PROCESS_QUERY_LIMITED_INFORMATION 的有效 handle；四個指標都指向
        // 本函式存活中的區域變數。
        // quality-allow: Win32 FFI，GetProcessTimes 沒有安全包裝
        unsafe { GetProcessTimes(self.0, &mut created, &mut exited, &mut kernel, &mut user) }?;
        Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }

    /// 等待失敗也視為已結束：開啟時已要求 SYNCHRONIZE，失敗代表 handle 不可用。
    fn has_exited(&self) -> bool {
        // SAFETY: self.0 是含 PROCESS_SYNCHRONIZE 的有效 handle；逾時 0 不會阻塞。
        // quality-allow: Win32 FFI，WaitForSingleObject 沒有安全包裝
        let state = unsafe { WaitForSingleObject(self.0, 0) };
        state != WAIT_TIMEOUT
    }

    /// 阻塞到程序結束。
    fn wait_exit(&self) -> windows::core::Result<()> {
        // SAFETY: self.0 是含 PROCESS_SYNCHRONIZE 的有效 handle，在 handle 存活期間等待。
        // quality-allow: Win32 FFI，WaitForSingleObject 沒有安全包裝
        let state = unsafe { WaitForSingleObject(self.0, INFINITE) };
        if state == WAIT_OBJECT_0 {
            Ok(())
        } else {
            Err(windows::core::Error::from_thread())
        }
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        // SAFETY: self.0 由 OpenProcess 取得，只在這裡關閉一次。
        // quality-allow: Win32 FFI；CloseHandle 失敗無可補救且 Drop 無法回傳錯誤
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn win_error(context: &str, e: windows::core::Error) -> AppError {
    AppError::Io {
        context: context.to_string(),
        message: e.message(),
    }
}

/// 以查詢與等待權限開啟程序；程序不存在回 `None`。
fn open_existing(pid: u32) -> AppResult<Option<ProcessHandle>> {
    match ProcessHandle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE) {
        Ok(handle) => Ok(Some(handle)),
        // 程序不存在時 OpenProcess 回 ERROR_INVALID_PARAMETER。
        Err(e) if e.code() == windows::core::HRESULT::from_win32(ERROR_INVALID_PARAMETER.0) => Ok(None),
        Err(e) => Err(win_error("開啟遊戲程序", e)),
    }
}

/// 取得仍在執行的程序身分；程序不存在或已結束回 `None`。
pub fn process_identity(pid: u32) -> AppResult<Option<GameProcess>> {
    let Some(handle) = open_existing(pid)? else {
        return Ok(None);
    };
    // 有人仍持有 handle 時，已結束的程序也開得起來，所以要另外確認。
    if handle.has_exited() {
        return Ok(None);
    }
    let created = handle
        .creation_time()
        .map_err(|e| win_error("讀取程序建立時間", e))?;
    Ok(Some(GameProcess { pid, created }))
}

/// 阻塞到 `p` 結束才回傳（`WaitForSingleObject`，不輪詢；呼叫端放在專用執行緒）。`p` 已不存在、或 pid 已被
/// 別的程序重用（建立時間不符）時立刻回傳。
pub fn wait_for_exit(p: &GameProcess) -> AppResult<()> {
    let Some(handle) = open_existing(p.pid)? else {
        return Ok(());
    };
    let created = handle
        .creation_time()
        .map_err(|e| win_error("讀取程序建立時間", e))?;
    if created != p.created {
        return Ok(());
    }
    handle.wait_exit().map_err(|e| win_error("等待遊戲程序結束", e))
}

/// pid 與建立時間都相符且仍在執行。無法確認（例如權限不足）就不是本程式啟動的那個程序。
pub fn is_alive(p: &GameProcess) -> bool {
    matches!(process_identity(p.pid), Ok(Some(current)) if current == *p)
}

/// 只結束指定 pid（Injector 回報的遊戲程序）。
pub fn terminate(pid: u32) -> AppResult<()> {
    let handle =
        ProcessHandle::open(pid, PROCESS_TERMINATE).map_err(|e| win_error("開啟遊戲程序", e))?;
    // SAFETY: handle 含 PROCESS_TERMINATE 權限，且在 handle 存活期間呼叫。
    // quality-allow: Win32 FFI，TerminateProcess 沒有安全包裝
    unsafe { TerminateProcess(handle.0, 1) }.map_err(|e| win_error("結束遊戲程序", e))
}

type RunningMap = BTreeMap<String, GameProcess>;
/// profile_id → 使用中的遊戲程序；共用時多筆。
type ProfileUsers = BTreeMap<String, Vec<GameProcess>>;

/// 兩份紀錄的讀改寫鎖（見模組說明）；臨界區只有讀檔、改 map、寫檔，沒有會留下半更新狀態的 panic 點（寫檔是
/// 暫存檔＋rename），poison 後可直接沿用。
static RECORDS: Mutex<()> = Mutex::new(());

/// 檔案不存在＝沒有紀錄；格式錯誤回報錯誤而不是當成空的，避免把使用中的 profile 當成沒人用。
fn read_profile_records(file: &Path) -> AppResult<ProfileUsers> {
    match read_optional(file)? {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|e| AppError::Io {
            context: format!("解析 {}", file.file_name().unwrap_or_default().to_string_lossy()),
            message: e.to_string(),
        }),
        None => Ok(ProfileUsers::new()),
    }
}

/// `accounts_running.json` 只用於畫面顯示與 ACT 切回（沒有佔用判斷依賴它）：內容損毀（例如寫到一半斷電）視為
/// 沒有紀錄，下次寫入時整檔覆蓋。讀檔本身失敗仍回錯誤。
fn read_account_records(file: &Path) -> AppResult<RunningMap> {
    Ok(read_optional(file)?
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default())
}

/// 讀整個檔案；不存在回 `None`。
fn read_optional(file: &Path) -> AppResult<Option<Vec<u8>>> {
    match fs::read(file) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(AppError::io(
            format!("讀取 {}", file.file_name().unwrap_or_default().to_string_lossy()),
            e,
        )),
    }
}

/// 該 profile 是否正被至少一個仍在執行的遊戲使用；已結束的紀錄視為不存在。
pub fn profile_in_use(paths: &AppPaths, profile_id: &str) -> AppResult<bool> {
    Ok(read_profile_records(&paths.profiles_running_file())?
        .get(profile_id)
        .is_some_and(|users| users.iter().any(is_alive)))
}

/// 正被至少一個仍在執行的遊戲使用的 profile id（依 id 排序）；已結束的只略過不寫回（讀取端不持有 launch mutex）。
pub fn profiles_in_use(paths: &AppPaths) -> AppResult<Vec<String>> {
    Ok(read_profile_records(&paths.profiles_running_file())?
        .into_iter()
        .filter(|(_, users)| users.iter().any(is_alive))
        .map(|(id, _)| id)
        .collect())
}

/// 注入成功後記錄 profile 佔用；同一 profile 已有別的遊戲在用時並列記錄（共用）。
pub fn mark_running(paths: &AppPaths, profile_id: &str, process: &GameProcess) -> AppResult<()> {
    let _records = RECORDS.lock().unwrap_or_else(PoisonError::into_inner);
    let file = paths.profiles_running_file();
    let mut running = read_profile_records(&file)?;
    for users in running.values_mut() {
        users.retain(is_alive);
    }
    running.retain(|_, users| !users.is_empty());
    running.entry(profile_id.to_string()).or_default().push(*process);
    store::write_json_atomic(&file, &running)
}

/// 撤銷 [`mark_running`]（遊戲提早結束時）：只刪 `process` 那一筆，同一 profile 的其他遊戲不動。
pub fn unmark_running(paths: &AppPaths, profile_id: &str, process: &GameProcess) -> AppResult<()> {
    let _records = RECORDS.lock().unwrap_or_else(PoisonError::into_inner);
    let file = paths.profiles_running_file();
    let mut running = read_profile_records(&file)?;
    let Some(users) = running.get_mut(profile_id) else {
        return Ok(());
    };
    let before = users.len();
    users.retain(|p| p != process);
    if users.len() == before {
        return Ok(());
    }
    if users.is_empty() {
        running.remove(profile_id);
    }
    store::write_json_atomic(&file, &running)
}

/// 遊戲啟動成功（帶不帶衛月）後記錄帳號對應的遊戲程序；順便清掉已結束的紀錄。
pub fn mark_account_running(
    paths: &AppPaths,
    account_id: &str,
    process: &GameProcess,
) -> AppResult<()> {
    let _records = RECORDS.lock().unwrap_or_else(PoisonError::into_inner);
    let file = paths.accounts_running_file();
    let mut running = read_account_records(&file)?;
    running.retain(|_, p| is_alive(p));
    running.insert(account_id.to_string(), *process);
    store::write_json_atomic(&file, &running)
}

/// 撤銷 [`mark_account_running`]（遊戲提早結束時）；紀錄已換成別的程序（例如之後又登入）就不動。
pub fn unmark_account_running(
    paths: &AppPaths,
    account_id: &str,
    process: &GameProcess,
) -> AppResult<()> {
    let _records = RECORDS.lock().unwrap_or_else(PoisonError::into_inner);
    let file = paths.accounts_running_file();
    let mut running = read_account_records(&file)?;
    if running.get(account_id) != Some(process) {
        return Ok(());
    }
    running.remove(account_id);
    store::write_json_atomic(&file, &running)
}

/// 遊戲仍在執行的帳號 id 與遊戲 pid（依帳號 id 排序）；已結束的只略過不寫回（讀取端不持有 launch mutex）。
pub fn running_account_pids(paths: &AppPaths) -> AppResult<Vec<(String, u32)>> {
    Ok(read_account_records(&paths.accounts_running_file())?
        .into_iter()
        .filter(|(_, p)| is_alive(p))
        .map(|(key, p)| (key, p.pid))
        .collect())
}

/// 帳號 id → 遊戲程序的全部紀錄，含已結束的（由呼叫端以 [`is_alive`] 判斷）。
pub fn account_processes(paths: &AppPaths) -> AppResult<BTreeMap<String, GameProcess>> {
    read_account_records(&paths.accounts_running_file())
}

/// 遊戲仍在執行的帳號 id。
pub fn running_accounts(paths: &AppPaths) -> AppResult<Vec<String>> {
    Ok(running_account_pids(paths)?
        .into_iter()
        .map(|(id, _)| id)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::DalamudChannel;

    fn profile(id: &str, data_dir: Option<PathBuf>) -> Profile {
        Profile {
            id: id.into(),
            name: id.into(),
            channel: DalamudChannel::Goatcorp,
            injection_delay_ms: 0,
            data_dir,
            share_confirmed: false,
        }
    }

    #[test]
    fn managed_profile_paths_live_under_profiles_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().join("app"));
        let p = ensure_profile_paths(&paths, &profile("p1", None)).expect("paths");
        let own = paths.profile_dir("p1");
        assert_eq!(p.config_file, own.join("dalamudConfig.json"));
        assert_eq!(p.plugin_dir, own.join("installedPlugins"));
        assert_eq!(p.dev_plugin_dir, own.join("devPlugins"));
        assert_eq!(p.log_dir, own.join("logs"));
        assert!(p.plugin_dir.is_dir() && p.dev_plugin_dir.is_dir() && p.log_dir.is_dir());
        assert!(ensure_profile_paths(&paths, &profile("..", None)).is_err());
    }

    #[test]
    fn external_data_dir_maps_config_and_plugins_but_keeps_own_logs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().join("app"));
        let external = dir.path().join("XIVLauncher");

        // 外部資料夾沒有 dalamudConfig.json：拒絕，且不建任何目錄。
        fs::create_dir_all(&external).expect("mkdir");
        let missing = ensure_profile_paths(&paths, &profile("p1", Some(external.clone())));
        assert!(matches!(missing, Err(AppError::NotFound { .. })));
        assert!(!external.join("installedPlugins").exists());

        fs::write(external.join("dalamudConfig.json"), b"{}").expect("write");
        let p = ensure_profile_paths(&paths, &profile("p1", Some(external.clone()))).expect("paths");
        assert_eq!(p.config_file, external.join("dalamudConfig.json"));
        assert_eq!(p.plugin_dir, external.join("installedPlugins"));
        assert_eq!(p.dev_plugin_dir, external.join("devPlugins"));
        assert_eq!(p.log_dir, paths.profile_dir("p1").join("logs"));
        assert!(p.plugin_dir.is_dir() && p.dev_plugin_dir.is_dir() && p.log_dir.is_dir());
        assert!(!external.join("logs").exists());
        assert_eq!(fs::read(&p.config_file).expect("read"), b"{}");
    }

    #[test]
    fn dead_records_are_ignored_and_pruned_on_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().to_path_buf());
        let me = process_identity(std::process::id())
            .expect("identity")
            .expect("current process is alive");
        let reused = GameProcess {
            pid: me.pid,
            created: me.created.wrapping_add(1),
        };
        let exited = GameProcess {
            pid: u32::MAX - 3,
            created: 1,
        };

        let mut seeded = ProfileUsers::new();
        seeded.insert("wrong-time".into(), vec![reused]);
        seeded.insert("no-such-pid".into(), vec![exited]);
        seeded.insert("alive".into(), vec![me]);
        // 共用中後開的那個先結束：先開的還在，仍算佔用。
        seeded.insert("shared".into(), vec![me, exited]);
        store::write_json_atomic(&paths.profiles_running_file(), &seeded).expect("seed");

        assert!(!profile_in_use(&paths, "wrong-time").expect("read"));
        assert!(!profile_in_use(&paths, "no-such-pid").expect("read"));
        assert!(!profile_in_use(&paths, "never-ran").expect("read"));
        assert!(profile_in_use(&paths, "alive").expect("read"));
        assert!(profile_in_use(&paths, "shared").expect("read"));
        assert_eq!(profiles_in_use(&paths).expect("read"), ["alive", "shared"]);

        mark_running(&paths, "new", &me).expect("write");
        let after = read_profile_records(&paths.profiles_running_file()).expect("read");
        assert_eq!(
            after.keys().map(String::as_str).collect::<Vec<_>>(),
            ["alive", "new", "shared"]
        );
        assert_eq!(after["shared"], [me]);

        // 再有遊戲用同一個 profile：並列記錄，不取代先開的。
        mark_running(&paths, "alive", &reused).expect("share");
        let shared = read_profile_records(&paths.profiles_running_file()).expect("read");
        assert_eq!(shared["alive"], [me, reused]);

        // 帳號紀錄同樣：讀取只回仍在執行者，寫入時清掉已結束者。
        let mut accounts = RunningMap::new();
        accounts.insert("wrong-time".into(), reused);
        accounts.insert("no-such-pid".into(), exited);
        accounts.insert("alive".into(), me);
        store::write_json_atomic(&paths.accounts_running_file(), &accounts).expect("seed");
        assert_eq!(running_accounts(&paths).expect("read"), ["alive"]);
        mark_account_running(&paths, "acc", &me).expect("write");
        assert_eq!(running_accounts(&paths).expect("read"), ["acc", "alive"]);
        assert_eq!(account_processes(&paths).expect("read").len(), 2);
    }

    /// 撤銷只刪同一程序的那一筆：共用中的其他遊戲、已換成別的程序（例如之後又登入）的帳號紀錄、其他項目都不動。
    #[test]
    fn unmark_removes_only_the_same_process() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().to_path_buf());
        let me = process_identity(std::process::id())
            .expect("identity")
            .expect("current process is alive");
        let earlier = GameProcess {
            pid: me.pid,
            created: me.created.wrapping_add(1),
        };
        mark_running(&paths, "p1", &me).expect("write");
        mark_running(&paths, "p2", &me).expect("write");
        // 最後寫：寫入前的剔除不會清掉這筆已結束的替身。
        mark_running(&paths, "p1", &earlier).expect("share");
        mark_account_running(&paths, "acc", &me).expect("write");

        unmark_running(&paths, "p1", &earlier).expect("one of shared");
        unmark_account_running(&paths, "acc", &earlier).expect("replaced");
        let profiles = read_profile_records(&paths.profiles_running_file()).expect("read");
        assert_eq!(profiles["p1"], [me]);
        assert_eq!(profiles["p2"], [me]);
        assert_eq!(running_accounts(&paths).expect("read"), ["acc"]);

        unmark_running(&paths, "p1", &earlier).expect("already gone");
        unmark_running(&paths, "never-ran", &me).expect("no record");
        unmark_running(&paths, "p1", &me).expect("same process");
        unmark_account_running(&paths, "acc", &me).expect("same process");
        unmark_account_running(&paths, "never-ran", &me).expect("no record");
        let profiles = read_profile_records(&paths.profiles_running_file()).expect("read");
        assert_eq!(profiles.keys().map(String::as_str).collect::<Vec<_>>(), ["p2"]);
        assert!(account_processes(&paths).expect("read").is_empty());
    }

    #[test]
    fn running_account_pids_lists_only_alive_records_with_their_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().to_path_buf());
        let me = process_identity(std::process::id())
            .expect("identity")
            .expect("current process is alive");

        assert!(running_account_pids(&paths).expect("no file").is_empty());

        let mut seeded = RunningMap::new();
        seeded.insert(
            "reused-pid".into(),
            GameProcess {
                pid: me.pid,
                created: me.created.wrapping_add(1),
            },
        );
        seeded.insert(
            "exited".into(),
            GameProcess {
                pid: u32::MAX - 3,
                created: 1,
            },
        );
        seeded.insert("b-alive".into(), me);
        seeded.insert("a-alive".into(), me);
        store::write_json_atomic(&paths.accounts_running_file(), &seeded).expect("seed");

        assert_eq!(
            running_account_pids(&paths).expect("read"),
            [("a-alive".to_string(), me.pid), ("b-alive".to_string(), me.pid)]
        );
        assert_eq!(running_accounts(&paths).expect("read"), ["a-alive", "b-alive"]);
    }

    #[test]
    fn corrupt_account_records_read_as_empty_and_are_overwritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().to_path_buf());
        let me = process_identity(std::process::id())
            .expect("identity")
            .expect("current process is alive");
        let file = paths.accounts_running_file();
        fs::create_dir_all(file.parent().expect("state dir")).expect("mkdir");
        fs::write(&file, b"{\"acc\":{\"pid\":1,").expect("write truncated");

        assert!(running_account_pids(&paths).expect("corrupt reads as empty").is_empty());
        assert!(account_processes(&paths).expect("corrupt reads as empty").is_empty());
        mark_account_running(&paths, "acc", &me).expect("overwrite");
        assert_eq!(running_accounts(&paths).expect("read"), ["acc"]);
    }

    #[test]
    fn corrupt_profile_records_stay_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().to_path_buf());
        let me = process_identity(std::process::id())
            .expect("identity")
            .expect("current process is alive");
        let file = paths.profiles_running_file();
        fs::create_dir_all(file.parent().expect("state dir")).expect("mkdir");
        fs::write(&file, b"{\"p1\":{\"pid\":1,").expect("write truncated");

        assert!(profile_in_use(&paths, "p1").is_err(), "佔用判斷不把損毀當成沒人用");
        assert!(mark_running(&paths, "p1", &me).is_err(), "不覆蓋損毀的佔用紀錄");
        assert_eq!(fs::read(&file).expect("read"), b"{\"p1\":{\"pid\":1,");
    }

    #[test]
    fn wait_for_exit_returns_when_the_game_ends_or_the_pid_was_reused() {
        let me = process_identity(std::process::id())
            .expect("identity")
            .expect("current process is alive");
        wait_for_exit(&GameProcess { pid: me.pid, created: me.created.wrapping_add(1) }).expect("reused pid");
        wait_for_exit(&GameProcess { pid: u32::MAX - 3, created: 1 }).expect("no such pid");

        // 遊戲替身：等一個不會出現的訊號（最多 120 秒）。
        let system_root = std::env::var_os("SystemRoot").expect("SystemRoot");
        let mut child = std::process::Command::new(Path::new(&system_root).join(r"System32\waitfor.exe"))
            .args(["/t", "120", &format!("xlm{}", uuid::Uuid::new_v4().simple())])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start fake game");
        let game = process_identity(child.id()).expect("identity").expect("fake game alive");
        let waiter = std::thread::spawn(move || wait_for_exit(&game));
        child.kill().expect("kill");
        waiter.join().expect("join").expect("wait");
        child.wait().expect("reap");
        assert!(!is_alive(&game));
    }
}
