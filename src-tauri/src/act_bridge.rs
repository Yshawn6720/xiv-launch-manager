//! ACT 連動（protocols §6）：讓 ACT 的 FFXIV_ACT_Plugin 自動選本程式剛啟動的遊戲程序，ACT 端的設定由本程式
//! 一鍵完成，不需要系統管理員權限。
//!
//! - 指令檔 `state/act_target.json`（§6.2）：遊戲啟動成功後只更新該區服那一項、保留另一項。ACT 內的外掛
//!   `XlmActBridge.cs` 依自己那個 ACT 目前的 Region 取對應項，核對程序名稱與建立時間後才切換 Process ID。
//!   指令檔的目標遊戲結束時切回同服仍開著、有勾「ACT 跟這個帳號」的帳號中最晚開的遊戲（[`ActTargets`]）。
//! - 外掛檔（§6.3）：嵌入的 `XlmActBridge.cs` 寫在 `%LOCALAPPDATA%\xiv-launch-manager\act\`，不寫 ACT 的資料夾。
//! - 啟用 ACT 連動（§6.4）：把外掛登記到每個已設定 ACT 的設定檔 `<ActPlugins>` 最後。
//! - 雙開（§6.5）：以主 ACT 為底，在同一處建立另一服用的可攜式 ACT，並寫回本程式的 ACT 設定；OverlayPlugin 的
//!   CEF 以 junction 共用主 ACT 那份。
//! - 兩個設定動作執行前都確認沒有 ACT 在跑：ACT 結束時以記憶體中的設定整檔覆寫設定檔。
//! - ACT 選單：兩服 ACT 下拉選單的選項（系統安裝的 ACT、雙開 ACT、設定裡的其他路徑）。
//! - FFXIV 插件檔（§6.6）：各 ACT 登記的 FFXIV_ACT_Plugin.dll；版本與最新版的比較見 `act_plugin`。

use std::collections::{BTreeMap, HashSet};
use std::fmt::Display;
use std::fs::{self, OpenOptions};
use std::io;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

use serde::{Deserialize, Serialize};

use crate::act;
use crate::dalamud::{self, GameProcess};
use crate::error::{AppError, AppResult};
use crate::patch::running;
use crate::region::Region;
use crate::store::{
    self, Account, ActConfig, ActSettings, AppPaths, TEMP_SUFFIX, with_suffix, write_new, write_replace,
};

/// 外掛檔名。
const PLUGIN_FILE: &str = "XlmActBridge.cs";
/// 外掛原始碼（ACT 在執行期編譯載入）。
const PLUGIN_SOURCE: &str = include_str!("../resources/act/XlmActBridge.cs");
/// 視為 ACT 的程序映像名稱（小寫）。
const ACT_IMAGE: &str = "advanced combat tracker.exe";
const FFXIV_PLUGIN_DLL: &str = "FFXIV_ACT_Plugin.dll";
/// FFXIV 插件的設定檔，在 ACT 設定檔的同一個資料夾（插件的 `SettingsPropertyPage`）。
const FFXIV_CONFIG_FILE: &str = "FFXIV_ACT_Plugin.config.xml";
/// 雙開實例裡，FFXIV 插件副本與 log 的位置（相對實例資料夾）。
const INSTANCE_PLUGIN_DIR: [&str; 2] = ["Plugins", "FFXIV_ACT_Plugin"];
const LOG_DIR: &str = "FFXIVLogs";
const CONFIG_DIR: &str = "Config";
/// OverlayPlugin 的 CEF（CefSharp，約 220MB），在 ACT 的 AppDataFolder 下；雙開 ACT 以 junction 共用主 ACT 那份。
/// 同層的 `OverlayPluginCache`（CEF 的 cache）不共用：兩個 ACT 同時執行不能用同一份。
const CEF_DIR: &str = "OverlayPluginCef";
/// 第一次修改 ACT 設定檔前留下的備份：`<設定檔名>.xlm-backup`。
const BACKUP_SUFFIX: &str = ".xlm-backup";
const BOM: &[u8] = b"\xEF\xBB\xBF";

/// ACT 連動用到的本機位置；正式環境由 [`Env::from_env`] 取自環境變數，測試指向臨時資料夾。
pub struct Env {
    /// `%APPDATA%`：主安裝 ACT 的設定在其下（`act::config_file`）。
    pub appdata: PathBuf,
    /// `%LOCALAPPDATA%\xiv-launch-manager\act`：外掛檔與雙開實例。
    pub home: PathBuf,
    /// 視為 ACT 的程序映像名稱（小寫）；任一在執行就拒絕設定。
    pub act_images: HashSet<String>,
}

impl Env {
    pub fn from_env() -> AppResult<Self> {
        let var = |name: &str| {
            std::env::var_os(name).map(PathBuf::from).ok_or_else(|| AppError::NotFound {
                what: format!("{name} 環境變數"),
            })
        };
        Ok(Self {
            appdata: var("APPDATA")?,
            home: var("LOCALAPPDATA")?.join("xiv-launch-manager").join("act"),
            act_images: HashSet::from([ACT_IMAGE.to_string()]),
        })
    }

    fn plugin_file(&self) -> PathBuf {
        self.home.join(PLUGIN_FILE)
    }

    /// 本程式為 `region` 建立的雙開 ACT 資料夾。
    fn instance_dir(&self, region: Region) -> PathBuf {
        self.home.join(match region {
            Region::Global => "ACT-Global",
            Region::Tw => "ACT-TW",
        })
    }

    /// `exe` 是本程式建立的雙開 ACT 時，回傳它服務的區服。
    fn instance_region(&self, exe: &Path) -> Option<Region> {
        let dir = exe.parent()?;
        [Region::Global, Region::Tw]
            .into_iter()
            .find(|&region| act::same_location(dir, &self.instance_dir(region)))
    }
}

// ---------- 指令檔（protocols §6.2） ----------

/// `state/act_target.json`：區服 → 遊戲程序（pid＋建立時間 FILETIME）。沒有資料的區服省略 key。
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    global: Option<GameProcess>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tw: Option<GameProcess>,
}

impl TargetFile {
    fn get(&self, region: Region) -> Option<GameProcess> {
        match region {
            Region::Global => self.global,
            Region::Tw => self.tw,
        }
    }

    fn set(&mut self, region: Region, process: GameProcess) {
        match region {
            Region::Global => self.global = Some(process),
            Region::Tw => self.tw = Some(process),
        }
    }
}

/// 只更新 `region` 那一項、保留另一項。既有檔案不存在、讀不到或格式不符時只寫這一項（protocols §6.2）。
/// 以 [`store::write_json_atomic`] 寫入（UTF-8、無 BOM，落盤後才返回）。呼叫端須持有 [`ActTargets`] 的鎖。
fn write_target(paths: &AppPaths, region: Region, process: &GameProcess) -> AppResult<()> {
    let file = paths.act_target_file();
    let mut target = read_target(&file).unwrap_or_default();
    target.set(region, *process);
    store::write_json_atomic(&file, &target)
}

/// 既有的指令檔；不存在、讀不到或格式不符都是 `None`，由 [`write_target`] 整檔改寫（只是連動用的暫態資料）。
fn read_target(file: &Path) -> Option<TargetFile> {
    serde_json::from_slice(&fs::read(file).ok()?).ok()
}

/// compare-and-swap：指令檔 `region` 那一項仍是 `ended`（pid 與建立時間都相同）時，才以 `next` 選出新目標並
/// 換上、保留另一項；回傳寫入的目標。已被換成別的程序、檔案不存在或讀不到時不選也不寫（回 `None`）。
/// 呼叫端須持有 [`ActTargets`] 的鎖，讀－比對－寫才不會與登入寫入交錯。
fn replace_target(
    file: &Path,
    region: Region,
    ended: &GameProcess,
    next: impl FnOnce() -> AppResult<Option<GameProcess>>,
) -> AppResult<Option<GameProcess>> {
    let Some(mut target) = read_target(file) else {
        return Ok(None);
    };
    if target.get(region) != Some(*ended) {
        return Ok(None);
    }
    let Some(next) = next()? else {
        return Ok(None);
    };
    target.set(region, next);
    store::write_json_atomic(file, &target)?;
    Ok(Some(next))
}

/// `ended` 結束後 ACT 要切回的遊戲：`region` 同服、勾了「ACT 跟這個帳號」、遊戲仍在執行（`alive`：pid 與
/// 建立時間都相符）的帳號中最晚開的那一個（`created` 最大）；`ended` 本身不算。`running` 是帳號 id → 遊戲程序
/// 的紀錄（`state/accounts_running.json`，可含已結束的）。
fn fallback_target(
    accounts: &[Account],
    running: &BTreeMap<String, GameProcess>,
    region: Region,
    ended: &GameProcess,
    alive: impl Fn(&GameProcess) -> bool,
) -> Option<GameProcess> {
    accounts
        .iter()
        .filter(|a| a.region == region && a.act_follow)
        .filter_map(|a| running.get(&a.id))
        .filter(|p| *p != ended && alive(p))
        .max_by_key(|p| p.created)
        .copied()
}

/// 指令檔的寫入者（protocols §6.2）。登入寫入與切回寫入共用一把鎖，讀－比對－寫都在鎖內完成；不用 launch
/// mutex（登入可能要好幾分鐘）。鎖內同時記著正在等待結束的遊戲，同一個程序只開一條等待執行緒。
#[derive(Clone)]
pub struct ActTargets(Arc<Shared>);

struct Shared {
    paths: AppPaths,
    /// 正在等待結束的遊戲；這把鎖也串行指令檔的讀寫。
    watching: Mutex<HashSet<GameProcess>>,
}

impl ActTargets {
    pub fn new(paths: AppPaths) -> Self {
        Self(Arc::new(Shared {
            paths,
            watching: Mutex::new(HashSet::new()),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, HashSet<GameProcess>> {
        // 臨界區只讀寫檔案與增刪 set，panic 不會留下半更新的狀態，poison 後可直接沿用。
        self.0.watching.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 遊戲啟動成功後：把 `region` 那一項換成 `process`（不比對，最新登入的優先），並開始等它結束。
    pub fn follow_login(&self, region: Region, process: &GameProcess) -> AppResult<()> {
        let mut watching = self.lock();
        write_target(&self.0.paths, region, process)?;
        self.watch(&mut watching, region, *process);
        Ok(())
    }

    /// 程式啟動時：對指令檔裡各區服的目標重新開始等待；目標已經結束的，等待立刻返回並切回。
    pub fn resume(&self) {
        let mut watching = self.lock();
        let Some(target) = read_target(&self.0.paths.act_target_file()) else {
            return;
        };
        for region in [Region::Global, Region::Tw] {
            if let Some(process) = target.get(region) {
                self.watch(&mut watching, region, process);
            }
        }
    }

    /// 開一條執行緒等 `process` 結束後切回（[`Self::switch_back`]）；已在等的程序不重複開。須持有鎖（`watching`）。
    fn watch(&self, watching: &mut HashSet<GameProcess>, region: Region, process: GameProcess) {
        if !watching.insert(process) {
            return;
        }
        let this = self.clone();
        let spawned = thread::Builder::new().name("act-target-watch".into()).spawn(move || {
            match dalamud::wait_for_exit(&process) {
                Ok(()) => this.switch_back(region, &process),
                Err(e) => log_error(region, "無法等待遊戲結束，這個遊戲關掉時不會切回", &e),
            }
            this.lock().remove(&process);
        });
        if let Err(e) = spawned {
            watching.remove(&process);
            log_error(region, "無法等待遊戲結束，這個遊戲關掉時不會切回", &e);
        }
    }

    /// `ended` 結束後：指令檔 `region` 那一項仍是它時，換成 [`fallback_target`] 選出的遊戲並等它結束；已被換成別的
    /// 程序（例如之後又登入、或另一條等待已切回）就不動。
    fn switch_back(&self, region: Region, ended: &GameProcess) {
        let paths = &self.0.paths;
        let mut watching = self.lock();
        let replaced = replace_target(&paths.act_target_file(), region, ended, || {
            let config = store::load_config(paths)?;
            let running = dalamud::account_processes(paths)?;
            Ok(fallback_target(&config.accounts, &running, region, ended, dalamud::is_alive))
        });
        match replaced {
            Ok(Some(next)) => self.watch(&mut watching, region, next),
            Ok(None) => {}
            Err(e) => log_error(region, "ACT 沒有切回其他遊戲", &e),
        }
    }
}

/// 背景等待與切回沒有畫面可回報，只記 log（stderr）；內容只有區服與錯誤訊息。
fn log_error(region: Region, what: &str, e: &dyn Display) {
    eprintln!("ACT 連動（{region}）：{what}：{e}");
}

// ---------- 外掛檔（protocols §6.3） ----------

/// 確保 `home\XlmActBridge.cs` 的內容等於嵌入版；不同或不存在就以暫存檔＋rename 改寫。ACT 啟動時才編譯外掛，
/// 改寫後 ACT 下次啟動生效。回傳外掛檔路徑。
pub fn ensure_plugin_file(home: &Path) -> AppResult<PathBuf> {
    let file = home.join(PLUGIN_FILE);
    if read_optional(&file, "讀取 ACT 連動外掛")?.as_deref() == Some(PLUGIN_SOURCE.as_bytes()) {
        return Ok(file);
    }
    fs::create_dir_all(home).map_err(|e| AppError::io("建立 ACT 連動資料夾", e))?;
    write_replace(&file, PLUGIN_SOURCE.as_bytes()).map_err(|e| AppError::io("寫入 ACT 連動外掛", e))?;
    Ok(file)
}

// ---------- 狀態（protocols §6.4） ----------

/// 外掛在一個 ACT 設定檔裡的狀態。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LinkState {
    /// 設定的執行檔不存在。
    ExeMissing,
    /// 設定檔不存在：這個 ACT 還沒開過。
    NeverOpened,
    /// 設定檔讀不懂（不是 UTF-8，或找不到外掛清單 `<ActPlugins>`）。
    Unreadable,
    NotRegistered,
    /// 有登記，但在 ACT 裡被取消勾選（`Enabled="False"`）。
    Disabled,
    Registered,
}

/// 一個已設定的 ACT；兩組設定指向同一個執行檔時合成一筆。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActLink {
    /// 用這個 ACT 的區服（國際服在前）。
    pub regions: Vec<Region>,
    pub exe: String,
    pub state: LinkState,
    /// 本程式建立的雙開 ACT。
    pub instance: bool,
}

/// 本程式建立、目前存在的雙開 ACT。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    pub region: Region,
    pub exe: String,
    /// 本程式該區服的 ACT 設定目前就是它。
    pub in_use: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkStatus {
    /// 已設定的 ACT（國際服在前，同一個執行檔只列一次）；都沒設定時為空。
    pub acts: Vec<ActLink>,
    /// 存在的雙開 ACT（國際服在前）。
    pub instances: Vec<Instance>,
}

/// 各 ACT 的登記狀態與雙開實例；只讀檔案，不列舉程序。
pub fn status(config: &ActConfig, env: &Env) -> AppResult<LinkStatus> {
    let plugin = env.plugin_file();
    let acts = configured(config)
        .into_iter()
        .map(|(exe, regions)| {
            Ok(ActLink {
                state: link_state(&exe, env, &plugin)?,
                instance: env.instance_region(&exe).is_some(),
                exe: exe.display().to_string(),
                regions,
            })
        })
        .collect::<AppResult<_>>()?;
    let instances = [Region::Global, Region::Tw]
        .into_iter()
        .filter_map(|region| {
            let exe = instance_exe(&env.instance_dir(region))?;
            let in_use = config.get(region).path.as_deref().is_some_and(|p| act::same_location(p, &exe));
            Some(Instance {
                region,
                exe: exe.display().to_string(),
                in_use,
            })
        })
        .collect();
    Ok(LinkStatus { acts, instances })
}

/// 已設定的 ACT 執行檔與使用它的區服（國際服、繁中服順序；同一個執行檔合成一筆）。
pub(crate) fn configured(config: &ActConfig) -> Vec<(PathBuf, Vec<Region>)> {
    let mut acts: Vec<(PathBuf, Vec<Region>)> = Vec::new();
    for region in [Region::Global, Region::Tw] {
        let Some(exe) = config.get(region).path.as_deref() else {
            continue;
        };
        match acts.iter_mut().find(|(known, _)| act::same_location(known, exe)) {
            Some((_, regions)) => regions.push(region),
            None => acts.push((exe.to_path_buf(), vec![region])),
        }
    }
    acts
}

fn link_state(exe: &Path, env: &Env, plugin: &Path) -> AppResult<LinkState> {
    Ok(match read_plugin_list(exe, env)? {
        ActPlugins::ExeMissing => LinkState::ExeMissing,
        ActPlugins::NeverOpened => LinkState::NeverOpened,
        ActPlugins::Unreadable => LinkState::Unreadable,
        ActPlugins::Found { exe_dir, list } => match list.registration(exe_dir, plugin) {
            Registration::Enabled => LinkState::Registered,
            Registration::Disabled(_) => LinkState::Disabled,
            Registration::Missing => LinkState::NotRegistered,
        },
    })
}

/// 一個 ACT 設定檔裡的外掛清單（[`read_plugin_list`]）。
enum ActPlugins<'a> {
    /// 設定的執行檔不存在。
    ExeMissing,
    /// 設定檔不存在：這個 ACT 還沒開過。
    NeverOpened,
    /// 設定檔讀不懂（不是 UTF-8，或找不到外掛清單 `<ActPlugins>`）。
    Unreadable,
    /// `exe_dir`：ACT 執行檔所在資料夾，清單中的相對路徑以它解析（[`resolve_act_path`]）。
    Found { exe_dir: &'a Path, list: PluginList },
}

/// 讀 `exe` 那個 ACT 的設定檔（位置依 `act::config_file`）裡的外掛清單；讀檔的 I/O 錯誤（不存在除外）回 Err。
fn read_plugin_list<'a>(exe: &'a Path, env: &Env) -> AppResult<ActPlugins<'a>> {
    let Some(exe_dir) = exe.parent().filter(|_| exe.is_file()) else {
        return Ok(ActPlugins::ExeMissing);
    };
    let config = act::config_file(exe, &env.appdata);
    let Some(bytes) = read_optional(&config, "讀取 ACT 設定檔")? else {
        return Ok(ActPlugins::NeverOpened);
    };
    Ok(match XmlFile::decode(&bytes, &config).and_then(|file| PluginList::find(&file.text)) {
        Err(_) => ActPlugins::Unreadable,
        Ok(list) => ActPlugins::Found { exe_dir, list },
    })
}

/// 雙開資料夾裡的 ACT 執行檔：頂層、帶自己設定檔（可攜式布局）的 `.exe`，依名稱排序取第一個。
fn instance_exe(dir: &Path) -> Option<PathBuf> {
    let mut exes: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) && act::is_portable(p))
        .collect();
    exes.sort();
    exes.into_iter().next()
}

// ---------- FFXIV 插件檔（protocols §6.6） ----------

/// 一個 ACT 登記的 FFXIV 插件檔（畫面顯示插件版本用，見 `act_plugin`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FfxivPlugin {
    /// 設定的執行檔不存在。
    ExeMissing,
    /// 設定檔不存在：這個 ACT 還沒開過。
    NeverOpened,
    /// 設定檔讀不到或讀不懂。
    Unreadable,
    /// 外掛清單裡沒有 FFXIV_ACT_Plugin.dll，或登記的檔案不存在。
    NotInstalled,
    /// 登記的 FFXIV_ACT_Plugin.dll（完整路徑，檔案存在）。
    Installed(PathBuf),
}

/// `exe` 那個 ACT 登記的 FFXIV 插件檔（[`PluginList::ffxiv_plugin`]；雙開 ACT 的相對路徑以它的 exe 資料夾解析）。
/// 只讀檔案；讀設定檔的 I/O 錯誤也算 `Unreadable`，不讓整個查詢失敗。
pub fn ffxiv_plugin(exe: &Path, env: &Env) -> FfxivPlugin {
    match read_plugin_list(exe, env) {
        Ok(ActPlugins::ExeMissing) => FfxivPlugin::ExeMissing,
        Ok(ActPlugins::NeverOpened) => FfxivPlugin::NeverOpened,
        Err(_) | Ok(ActPlugins::Unreadable) => FfxivPlugin::Unreadable,
        Ok(ActPlugins::Found { exe_dir, list }) => match list.ffxiv_plugin(exe_dir) {
            Some(dll) if dll.is_file() => FfxivPlugin::Installed(dll),
            _ => FfxivPlugin::NotInstalled,
        },
    }
}

// ---------- ACT 選單（擴充功能 → ACT 的兩服下拉選單） ----------

/// 選單項目的來源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ChoiceKind {
    /// 系統安裝的 ACT（`act::detect`）。
    Installed,
    /// 本程式建立的雙開 ACT。
    Dual,
    /// 設定裡已存、不是上面兩種的路徑。
    Custom,
}

/// ACT 下拉選單的一項。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActChoice {
    pub path: String,
    pub kind: ChoiceKind,
    /// 雙開 ACT 服務的區服；其他種類為 `None`。
    pub region: Option<Region>,
    /// 執行檔存在（系統安裝與雙開一定存在；自訂路徑可能已被移走）。
    pub exists: bool,
    /// 目前設定選的就是這一項的區服（國際服在前）；畫面據此選中，不必在前端比對路徑。
    pub selected_by: Vec<Region>,
}

/// 兩服 ACT 下拉選單的選項，依序為：系統安裝的 ACT（`installed`，偵測順序）、雙開 ACT（國際服、繁中服）、設定裡
/// 已存但不在前兩者的路徑（國際服、繁中服）。同一個執行檔只列一次（`act::same_location`：不分大小寫，存在時比
/// 最終路徑），保留先出現的那一項。只讀檔案。
pub fn choices(config: &ActConfig, env: &Env, installed: Vec<PathBuf>) -> Vec<ActChoice> {
    const REGIONS: [Region; 2] = [Region::Global, Region::Tw];
    let dual = REGIONS
        .into_iter()
        .filter_map(|region| Some((instance_exe(&env.instance_dir(region))?, ChoiceKind::Dual, Some(region))));
    let saved = REGIONS
        .into_iter()
        .filter_map(|region| config.get(region).path.clone())
        .map(|path| (path, ChoiceKind::Custom, None));
    let mut unique: Vec<(PathBuf, ChoiceKind, Option<Region>)> = Vec::new();
    for item in installed.into_iter().map(|path| (path, ChoiceKind::Installed, None)).chain(dual).chain(saved) {
        if !unique.iter().any(|(known, ..)| act::same_location(known, &item.0)) {
            unique.push(item);
        }
    }
    unique
        .into_iter()
        .map(|(path, kind, region)| ActChoice {
            selected_by: REGIONS
                .into_iter()
                .filter(|&r| config.get(r).path.as_deref().is_some_and(|p| act::same_location(p, &path)))
                .collect(),
            exists: path.is_file(),
            path: path.display().to_string(),
            kind,
            region,
        })
        .collect()
}

// ---------- 啟用 ACT 連動（protocols §6.4） ----------

/// 一個 ACT 設定檔的登記結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RegisterChange {
    /// 加在外掛清單最後。
    Added,
    /// 原本已登記但取消勾選，改回啟用。
    Reenabled,
    /// 原本就已啟用，設定檔沒有改。
    Unchanged,
}

/// 一個 ACT 的登記結果（畫面顯示用）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActChange {
    pub regions: Vec<Region>,
    pub exe: String,
    pub change: RegisterChange,
}

/// 「啟用 ACT 連動」：確認沒有 ACT 在跑、外掛檔對齊嵌入版後，登記到每個已設定 ACT（同一個執行檔只處理一次）
/// 的設定檔。某個 ACT 失敗就停在那裡並回報是哪一個（之前的已寫好，重按會略過）。
pub fn enable_link(config: &ActConfig, env: &Env) -> AppResult<Vec<ActChange>> {
    ensure_act_closed(env)?;
    let acts = configured(config);
    if acts.is_empty() {
        return Err(AppError::ActNotConfigured);
    }
    let plugin = ensure_plugin_file(&env.home)?;
    acts.into_iter()
        .map(|(exe, regions)| {
            let change = register_act(&exe, env, &plugin).map_err(|e| labelled(&regions, e))?;
            Ok(ActChange {
                regions,
                exe: exe.display().to_string(),
                change,
            })
        })
        .collect()
}

fn ensure_act_closed(env: &Env) -> AppResult<()> {
    if running::any_named(&env.act_images)? {
        return Err(AppError::ActRunning);
    }
    Ok(())
}

/// 把外掛登記到 `exe` 那個 ACT 的設定檔（位置依 `act::config_file`）。
fn register_act(exe: &Path, env: &Env, plugin: &Path) -> AppResult<RegisterChange> {
    act::validate_exe(exe)?;
    let config = act::config_file(exe, &env.appdata);
    let bytes = read_optional(&config, "讀取 ACT 設定檔")?.ok_or_else(|| never_opened(&config))?;
    let file = XmlFile::decode(&bytes, &config)?;
    let Some((change, text)) = register_text(&file.text, parent(exe)?, plugin)? else {
        return Ok(RegisterChange::Unchanged);
    };
    backup_once(&config, &bytes)?;
    write_replace(&config, &file.encode(&text)).map_err(|e| AppError::io("寫入 ACT 設定檔", e))?;
    Ok(change)
}

/// 第一次修改設定檔前，在同資料夾留一份原檔 `<檔名>.xlm-backup`；已存在就不覆寫（保留最早的原檔）。
fn backup_once(config: &Path, original: &[u8]) -> AppResult<()> {
    match write_new(&with_suffix(config, BACKUP_SUFFIX), original) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(AppError::io("備份 ACT 設定檔", e)),
    }
}

/// 外掛登記進 ACT 設定檔原文後的結果；已啟用就是 `None`（不改）。有登記但取消勾選：第一筆改成
/// `Enabled="True"`；沒有登記：加在 `<ActPlugins>` 最後（ACT 依清單順序載入，排在 FFXIV 插件之後）。
/// 其餘內容（BOM 以外的每個 byte、縮排、換行）不動。
fn register_text(xml: &str, exe_dir: &Path, plugin: &Path) -> AppResult<Option<(RegisterChange, String)>> {
    let list = PluginList::find(xml)?;
    let edit = match list.registration(exe_dir, plugin) {
        Registration::Enabled => return Ok(None),
        Registration::Disabled(tag) => (RegisterChange::Reenabled, enable_edit(tag)),
        Registration::Missing => (RegisterChange::Added, list.append_edit(xml, plugin)),
    };
    Ok(Some((edit.0, apply_edits(xml, vec![edit.1]))))
}

/// 把 `<Plugin>` 標籤改成 `Enabled="True"`：有 Enabled 屬性就換值，沒有就加在元素名稱之後。
fn enable_edit(tag: &Tag) -> (Range<usize>, String) {
    match tag.attr("Enabled") {
        Some(attr) => (attr.value.clone(), "True".into()),
        None => {
            let at = tag.span.start + 1 + tag.name.len();
            (at..at, r#" Enabled="True""#.into())
        }
    }
}

/// `<ActPlugins>` 區段：開始標籤、`</ActPlugins>` 的位置（自我結束時為 `None`）與其中的 `<Plugin>`。
struct PluginList {
    open: Tag,
    close: Option<usize>,
    plugins: Vec<Tag>,
}

/// 外掛在 `<ActPlugins>` 中的狀態；`Disabled` 帶第一筆指向外掛檔的 `<Plugin>`。
enum Registration<'a> {
    Enabled,
    Disabled(&'a Tag),
    Missing,
}

impl PluginList {
    fn find(xml: &str) -> AppResult<Self> {
        let unreadable = || setup_error("ACT 設定檔裡找不到外掛清單（ActPlugins），請先開過一次 ACT 再關閉".into());
        let mut tags = start_tags(xml).into_iter();
        let open = tags.by_ref().find(|t| t.name == "ActPlugins").ok_or_else(unreadable)?;
        if open.self_closing {
            return Ok(Self {
                open,
                close: None,
                plugins: Vec::new(),
            });
        }
        let close = xml[open.span.end..]
            .find("</ActPlugins")
            .map(|i| open.span.end + i)
            .ok_or_else(unreadable)?;
        let plugins = tags.take_while(|t| t.span.start < close).filter(|t| t.name == "Plugin").collect();
        Ok(Self {
            open,
            close: Some(close),
            plugins,
        })
    }

    /// `Path` 指向 `plugin` 的 `<Plugin>` 中有任一啟用＝`Enabled`。
    fn registration(&self, exe_dir: &Path, plugin: &Path) -> Registration<'_> {
        let mut first = None;
        for tag in self.plugins.iter().filter(|t| t.attr("Path").is_some_and(|p| points_to(&p.text, exe_dir, plugin))) {
            if tag.attr("Enabled").is_some_and(|a| a.text.trim().eq_ignore_ascii_case("true")) {
                return Registration::Enabled;
            }
            first.get_or_insert(tag);
        }
        first.map_or(Registration::Missing, Registration::Disabled)
    }

    /// FFXIV 插件的完整路徑：檔名為 FFXIV_ACT_Plugin.dll 的第一筆 `<Plugin>`（不論是否勾選），相對路徑依
    /// [`resolve_act_path`] 解析；不檢查檔案是否存在。
    fn ffxiv_plugin(&self, exe_dir: &Path) -> Option<PathBuf> {
        self.plugins
            .iter()
            .filter_map(|tag| tag.attr("Path"))
            .map(|path| resolve_act_path(&path.text, exe_dir))
            .find(|path| path.file_name().is_some_and(|n| n.eq_ignore_ascii_case(FFXIV_PLUGIN_DLL)))
    }

    /// 在清單最後加一筆 `<Plugin Enabled="True" Path="…" />`，縮排沿用既有 `<Plugin>`（沒有就比
    /// `<ActPlugins>` 多一層），換行沿用原文。
    fn append_edit(&self, xml: &str, plugin: &Path) -> (Range<usize>, String) {
        let nl = newline(xml);
        let entry = format!(r#"<Plugin Enabled="True" Path="{}" />"#, escape_attr(&plugin.to_string_lossy(), '"'));
        let indent = line_indent(xml, self.open.span.start);
        let child = self
            .plugins
            .first()
            .map(|t| line_indent(xml, t.span.start))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("{indent}{}", if indent.contains('\t') { "\t" } else { "    " }));
        let Some(close) = self.close else {
            return (
                self.open.span.clone(),
                format!("<ActPlugins>{nl}{child}{entry}{nl}{indent}</ActPlugins>"),
            );
        };
        let line_start = xml[..close].rfind('\n').map_or(0, |i| i + 1);
        if xml[line_start..close].trim().is_empty() {
            (line_start..line_start, format!("{child}{entry}{nl}"))
        } else {
            (close..close, format!("{nl}{child}{entry}{nl}{indent}"))
        }
    }
}

/// 設定檔中的 `raw`（`<Plugin Path>`）是否指向 `target`（不分大小寫，見 `act::same_location`）。
fn points_to(raw: &str, exe_dir: &Path, target: &Path) -> bool {
    act::same_location(&resolve_act_path(raw, exe_dir), target)
}

/// ACT 解析設定檔中路徑的方式（`MakeAbsolutePathString`）：不是 rooted（.NET `Path.IsPathRooted`：以 `\`、`/`
/// 或磁碟代號開頭）就接在 ACT 執行檔資料夾後，再消去 `.`／`..`。可攜式 ACT 存檔時把外掛路徑寫成相對路徑。
fn resolve_act_path(raw: &str, exe_dir: &Path) -> PathBuf {
    let joined = if is_rooted(raw) {
        PathBuf::from(raw)
    } else {
        exe_dir.join(raw)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn is_rooted(raw: &str) -> bool {
    raw.starts_with(['\\', '/']) || raw.as_bytes().get(1) == Some(&b':')
}

// ---------- 雙開（protocols §6.5） ----------

/// 「建立另一服的 ACT」的結果；設定已依此寫回。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DualResult {
    /// 主 ACT 的區服（它的 FFXIV 插件 Region）與執行檔。
    pub main_region: Region,
    pub main_exe: String,
    /// 另一服用的 ACT。
    pub instance_region: Region,
    pub instance_exe: String,
    /// false＝資料夾原本就在，沒有重建（只補登記與寫回設定）。
    pub created: bool,
}

/// 主 ACT 的檔案與設定（雙開的底）。
struct MainAct {
    exe: PathBuf,
    /// ACT 設定檔與其原文。
    config: PathBuf,
    act_xml: XmlFile,
    ffxiv_xml: XmlFile,
    region: Region,
    /// FFXIV_ACT_Plugin.dll 的完整路徑（相對路徑已解析）。
    ffxiv_dll: PathBuf,
}

/// 「建立另一服的 ACT」：以主 ACT（設定中第一個不是雙開實例、執行檔存在的 ACT）為底，在 `home` 建立另一服
/// （主 ACT 的 FFXIV 插件 Region 的相反）用的可攜式 ACT；已存在就不重建。主 ACT 的設定檔須能以目前權限寫入
/// （[`ensure_main_writable`]），否則什麼都不做就拒絕。兩個 ACT 都登記外掛，成功後寫回 `config`：主 ACT 對應它的
/// Region、新 ACT 對應另一服，「隨遊戲開啟」都沿用主 ACT 那一組。失敗時 `config` 不變，新資料夾不留半成品。
pub fn create_dual(config: &mut ActConfig, env: &Env) -> AppResult<DualResult> {
    ensure_act_closed(env)?;
    let (exe, launch_with_game) = main_act(config, env)?;
    let main = MainAct::read(exe, env)?;
    ensure_main_writable(&main.config)?;
    let plugin = ensure_plugin_file(&env.home)?;
    let other = other_region(main.region);
    let dir = env.instance_dir(other);
    let created = !dir.exists();
    let instance = if created {
        build_instance(&main, &dir, other, &plugin)?;
        dir.join(main.exe.file_name().unwrap_or_default())
    } else {
        let exe = instance_exe(&dir).ok_or_else(|| {
            setup_error(format!("{} 不是完整的 ACT，請刪除這個資料夾後再試一次", dir.display()))
        })?;
        link_cef(&main, &dir).map_err(|e| labelled(&[other], e))?;
        register_act(&exe, env, &plugin).map_err(|e| labelled(&[other], e))?;
        exe
    };
    register_act(&main.exe, env, &plugin).map_err(|e| labelled(&[main.region], e))?;
    *config.get_mut(main.region) = ActSettings {
        path: Some(main.exe.clone()),
        launch_with_game,
    };
    *config.get_mut(other) = ActSettings {
        path: Some(instance.clone()),
        launch_with_game,
    };
    Ok(DualResult {
        main_region: main.region,
        main_exe: main.exe.display().to_string(),
        instance_region: other,
        instance_exe: instance.display().to_string(),
        created,
    })
}

/// 設定中第一個（國際服、繁中服順序）不是雙開實例、執行檔存在的 ACT 與它那組的「隨遊戲開啟」。
fn main_act(config: &ActConfig, env: &Env) -> AppResult<(PathBuf, bool)> {
    let candidates: Vec<(&Path, bool)> = [Region::Global, Region::Tw]
        .into_iter()
        .map(|region| config.get(region))
        .filter_map(|s| s.path.as_deref().map(|p| (p, s.launch_with_game)))
        .filter(|(p, _)| env.instance_region(p).is_none())
        .collect();
    if let Some((exe, launch)) = candidates.iter().find(|(p, _)| p.is_file()) {
        return Ok((exe.to_path_buf(), *launch));
    }
    match candidates.first() {
        Some((exe, _)) => Err(act::validate_exe(exe).err().unwrap_or(AppError::ActNotConfigured)),
        None => Err(AppError::ActNotConfigured),
    }
}

/// 建立或補登記雙開 ACT 前，確認主 ACT 的設定檔能以目前權限改寫：以寫入權限開啟設定檔（不截斷、不寫入），並在
/// 同資料夾建立再刪除一個唯一暫存檔（登記時的暫存檔＋取代兩者都要）。主 ACT 的內容不變。寫不進去的主 ACT（例如
/// 可攜式 ACT 裝在 Program Files、設定檔唯讀）受系統保護；以它為底複製到使用者可寫的 `%LOCALAPPDATA%` 會降低
/// 它的保護，所以在建立任何東西之前拒絕，也不會留下登記不了主 ACT 的孤兒實例。
fn ensure_main_writable(config: &Path) -> AppResult<()> {
    let refused = |e: io::Error| {
        setup_error(format!(
            "目前的權限無法寫入主 ACT 的設定檔 {}（{e}），不建立另一服的 ACT：雙開 ACT 放在使用者可寫入的資料夾，以受保護的 ACT 為底會降低它的保護。請改用設定檔可以寫入的 ACT（例如安裝版，設定在 %APPDATA%），或取消設定檔的唯讀屬性後再試一次",
            config.display()
        ))
    };
    OpenOptions::new().write(true).open(config).map_err(refused)?;
    let probe = with_suffix(config, &format!(".{}{TEMP_SUFFIX}", uuid::Uuid::new_v4().simple()));
    write_new(&probe, &[]).map_err(refused)?;
    // quality-allow: 探測檔清理是 best-effort；刪不掉只留下以 .xlm-tmp 結尾的空檔，複製實例時略過
    let _ = fs::remove_file(&probe);
    Ok(())
}

impl MainAct {
    /// 讀主 ACT 的設定檔與 FFXIV 插件設定檔，確認 Region 已選國際或繁中、外掛清單裡有 FFXIV 插件。
    fn read(exe: PathBuf, env: &Env) -> AppResult<Self> {
        let config = act::config_file(&exe, &env.appdata);
        let bytes = read_optional(&config, "讀取 ACT 設定檔")?.ok_or_else(|| never_opened(&config))?;
        let act_xml = XmlFile::decode(&bytes, &config)?;
        let ffxiv_path = parent(&config)?.join(FFXIV_CONFIG_FILE);
        let region_not_set = || {
            setup_error(
                "請先在 ACT 的 FFXIV 插件選好 Region（國際服選 Global、繁中服選 繁體中文），關閉 ACT 後再試一次".into(),
            )
        };
        let ffxiv_bytes = read_optional(&ffxiv_path, "讀取 FFXIV 插件設定檔")?.ok_or_else(region_not_set)?;
        let ffxiv_xml = XmlFile::decode(&ffxiv_bytes, &ffxiv_path)?;
        let tags = start_tags(&ffxiv_xml.text);
        let region = setting(&tags, "cboRegion")
            .and_then(|tag| tag.attr("Value"))
            .and_then(|value| region_from_text(&value.text))
            .ok_or_else(region_not_set)?;
        let ffxiv_dll = ffxiv_plugin_path(&act_xml.text, parent(&exe)?)?;
        Ok(Self {
            exe,
            config,
            act_xml,
            ffxiv_xml,
            region,
            ffxiv_dll,
        })
    }

    /// ACT 的 AppDataFolder（設定檔所在 `Config\` 的上一層）：主安裝為 `%APPDATA%\Advanced Combat Tracker`，可攜式為
    /// exe 資料夾。
    fn data_dir(&self) -> AppResult<&Path> {
        parent(parent(&self.config)?)
    }
}

/// 主 ACT 外掛清單中 FFXIV 插件的完整路徑（[`PluginList::ffxiv_plugin`]），檔案須存在。
fn ffxiv_plugin_path(xml: &str, exe_dir: &Path) -> AppResult<PathBuf> {
    let dll = PluginList::find(xml)?
        .ffxiv_plugin(exe_dir)
        .ok_or_else(|| setup_error("主 ACT 還沒加入 FFXIV 插件（FFXIV_ACT_Plugin.dll），請先在 ACT 裝好 FFXIV 插件".into()))?;
    if !dll.is_file() {
        return Err(setup_error(format!("找不到主 ACT 的 FFXIV 插件檔 {}", dll.display())));
    }
    Ok(dll)
}

/// 在 `home` 下的暫存資料夾建好實例，全部成功才 rename 成 `dir`；任何一步失敗就刪掉暫存資料夾，不留半成品。
/// 設定檔中的路徑一律寫 `dir`（rename 後的位置）。
fn build_instance(main: &MainAct, dir: &Path, region: Region, plugin: &Path) -> AppResult<()> {
    let home = parent(dir)?;
    fs::create_dir_all(home).map_err(|e| AppError::io("建立 ACT 連動資料夾", e))?;
    let name = dir.file_name().unwrap_or_default().to_string_lossy();
    let tmp = home.join(format!(".{name}.{}{TEMP_SUFFIX}", uuid::Uuid::new_v4().simple()));
    fs::create_dir(&tmp).map_err(|e| AppError::io("建立暫存資料夾", e))?;
    let result = fill_instance(main, &tmp, dir, region, plugin)
        .and_then(|()| fs::rename(&tmp, dir).map_err(|e| AppError::io("建立雙開 ACT 資料夾", e)));
    if result.is_err() {
        // 暫存資料夾裡的 OverlayPluginCef 是 junction：std 的 remove_dir_all 只刪連結本身、不進入主 ACT 的 CEF
        // （測試 `instance_cleanup_keeps_main_cef` 實證）。
        // quality-allow: 清除是 best-effort；刪不掉只留下以 . 開頭的暫存資料夾，正式資料夾不存在，下次會重建
        let _ = fs::remove_dir_all(&tmp);
    }
    result
}

/// 複製主 ACT 的頂層檔案、設定資料夾的頂層檔案（`Backup\` 是資料夾，不在內）與 FFXIV 插件，建立 log 資料夾與
/// OverlayPlugin CEF 的 junction，再改寫副本的兩個設定檔並登記外掛。
fn fill_instance(main: &MainAct, tmp: &Path, dir: &Path, region: Region, plugin: &Path) -> AppResult<()> {
    let main_dir = parent(&main.exe)?;
    copy_files(main_dir, tmp, |_| true)?;
    let config_dir = tmp.join(CONFIG_DIR);
    fs::create_dir(&config_dir).map_err(|e| AppError::io("建立 Config 資料夾", e))?;
    copy_files(parent(&main.config)?, &config_dir, |_| true)?;
    let plugin_dir = INSTANCE_PLUGIN_DIR.iter().fold(tmp.to_path_buf(), |p, part| p.join(part));
    fs::create_dir_all(&plugin_dir).map_err(|e| AppError::io("建立 Plugins 資料夾", e))?;
    copy_files(parent(&main.ffxiv_dll)?, &plugin_dir, |name| {
        let lower = name.to_lowercase();
        lower.ends_with(".dll") && (lower.starts_with("ffxiv_act_plugin") || lower.starts_with("deucalion"))
    })?;
    fs::create_dir(tmp.join(LOG_DIR)).map_err(|e| AppError::io("建立 FFXIVLogs 資料夾", e))?;
    link_cef(main, tmp)?;

    let logs = dir.join(LOG_DIR);
    let dll = INSTANCE_PLUGIN_DIR
        .iter()
        .fold(dir.to_path_buf(), |p, part| p.join(part))
        .join(main.ffxiv_dll.file_name().unwrap_or_default());
    let ffxiv_text = rewrite_ffxiv_config(&main.ffxiv_xml.text, region, &logs)?;
    let old_logs = old_log_dirs(main)?;
    let act_text = rewrite_act_config(&main.act_xml.text, main, &old_logs, &logs, &dll)?;
    let act_text = match register_text(&act_text, dir, plugin)? {
        Some((_, text)) => text,
        None => act_text,
    };
    let write = |name: &str, bytes: Vec<u8>| {
        fs::write(config_dir.join(name), bytes).map_err(|e| AppError::io(format!("寫入 {name}"), e))
    };
    write(&act::config_file_name(&main.exe), main.act_xml.encode(&act_text))?;
    write(FFXIV_CONFIG_FILE, main.ffxiv_xml.encode(&ffxiv_text))
}

/// 主 ACT 的 AppDataFolder 有 `OverlayPluginCef` 時，在 `dir` 建立指向它的 junction（一般權限即可），OverlayPlugin
/// 就不會在雙開 ACT 另外下載一份 CEF。`dir` 已有同名項目（使用者自己下載的實體資料夾或既有 junction）就不動；
/// 主 ACT 沒有就略過。
fn link_cef(main: &MainAct, dir: &Path) -> AppResult<()> {
    let source = main.data_dir()?.join(CEF_DIR);
    let link = dir.join(CEF_DIR);
    match fs::symlink_metadata(&link) {
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(AppError::io(format!("讀取 {}", link.display()), e)),
    }
    if !source.is_dir() {
        return Ok(());
    }
    junction::create(&source, &link).map_err(|e| {
        // junction::create 先建空資料夾再設 reparse point；後者失敗時留下的空資料夾會讓下次誤以為「已有」。
        if e.kind() != io::ErrorKind::AlreadyExists {
            // quality-allow: 只刪空資料夾（remove_dir 不刪有內容的資料夾、不跟隨連結）；刪不掉時錯誤已回報
            let _ = fs::remove_dir(&link);
        }
        AppError::io(format!("建立 {} 指向主 ACT 的 OverlayPluginCef", link.display()), e)
    })
}

/// 主 ACT 的 log 資料夾：FFXIV 插件的 `txtLogFileDirectory`（空白時是插件預設的 `<AppDataFolder>\FFXIVLogs`）。
/// 改寫副本時，等於或位於其下的設定值都換到新 ACT 的 log 資料夾。
fn old_log_dirs(main: &MainAct) -> AppResult<Vec<String>> {
    let tags = start_tags(&main.ffxiv_xml.text);
    let value = setting(&tags, "txtLogFileDirectory")
        .and_then(|tag| tag.attr("Value"))
        .map(|v| v.text.trim().to_string())
        .ok_or_else(|| missing_field(FFXIV_CONFIG_FILE, "txtLogFileDirectory"))?;
    if !value.is_empty() {
        return Ok(vec![value]);
    }
    Ok(vec![main.data_dir()?.join(LOG_DIR).display().to_string()])
}

/// 副本的 FFXIV 插件設定：`cboRegion` 改成另一服的選項文字，`txtLogFileDirectory` 改成新 ACT 的 log 資料夾。
fn rewrite_ffxiv_config(xml: &str, region: Region, logs: &Path) -> AppResult<String> {
    let tags = start_tags(xml);
    let mut edits = Vec::new();
    for (name, value) in [("cboRegion", region_text(region).to_string()), ("txtLogFileDirectory", logs.display().to_string())] {
        let attr = setting(&tags, name)
            .and_then(|tag| tag.attr("Value"))
            .ok_or_else(|| missing_field(FFXIV_CONFIG_FILE, name))?;
        edits.push((attr.value.clone(), escape_attr(&value, attr.quote)));
    }
    Ok(apply_edits(xml, edits))
}

/// 副本的 ACT 設定：FFXIV 插件的 `<Plugin Path>` 指向副本；其他相對路徑的 `<Plugin Path>` 改成以主 ACT 資料夾
/// 解析後的完整路徑（副本只有頂層檔案，沿用主 ACT 的其他插件）；`folderLogs` 與等於或位於舊 log 資料夾下的
/// 設定值改到新 ACT 的 log 資料夾（ACT 啟動時會開 `logFilePath`，不能讀到主 ACT 的 log）。
fn rewrite_act_config(xml: &str, main: &MainAct, old_logs: &[String], logs: &Path, dll: &Path) -> AppResult<String> {
    let main_dir = parent(&main.exe)?;
    let list = PluginList::find(xml)?;
    let mut edits = Vec::new();
    let mut ffxiv_found = false;
    for attr in list.plugins.iter().filter_map(|tag| tag.attr("Path")) {
        let resolved = resolve_act_path(&attr.text, main_dir);
        let new_path = if act::same_location(&resolved, &main.ffxiv_dll) {
            ffxiv_found = true;
            dll.to_path_buf()
        } else if !is_rooted(&attr.text) {
            resolved
        } else {
            continue;
        };
        edits.push((attr.value.clone(), escape_attr(&new_path.to_string_lossy(), attr.quote)));
    }
    if !ffxiv_found {
        return Err(missing_field(&act::config_file_name(&main.exe), FFXIV_PLUGIN_DLL));
    }
    let logs_text = logs.display().to_string();
    for tag in start_tags(xml).iter().filter(|t| t.name != "Plugin") {
        let Some(value) = tag.attr("Value") else {
            continue;
        };
        let replacement = if tag.attr("Name").is_some_and(|n| n.text == "folderLogs") {
            Some(logs_text.clone())
        } else {
            old_logs.iter().find_map(|old| relocate(&value.text, old, &logs_text))
        };
        if let Some(text) = replacement {
            edits.push((value.value.clone(), escape_attr(&text, value.quote)));
        }
    }
    Ok(apply_edits(xml, edits))
}

/// `value` 等於 `old` 或位於其下時，換成 `new` 開頭的路徑（不分大小寫，`/` 視同 `\`）。
fn relocate(value: &str, old: &str, new: &str) -> Option<String> {
    let value = value.replace('/', "\\");
    let old = old.replace('/', "\\");
    let old = old.trim_end_matches('\\');
    if old.is_empty() || value.len() < old.len() || !value.is_char_boundary(old.len()) {
        return None;
    }
    let (head, tail) = value.split_at(old.len());
    let under = tail.trim_end_matches('\\').is_empty() || tail.starts_with('\\');
    (under && head.to_lowercase() == old.to_lowercase()).then(|| match tail.trim_end_matches('\\') {
        "" => new.to_string(),
        rest => format!("{new}{rest}"),
    })
}

/// 把 `from` 的頂層一般檔案（不含子資料夾、本模組的備份與暫存檔）中 `keep` 為真者複製到 `to`。
fn copy_files(from: &Path, to: &Path, keep: impl Fn(&str) -> bool) -> AppResult<()> {
    let read_error = |e| AppError::io(format!("讀取 {}", from.display()), e);
    for entry in fs::read_dir(from).map_err(read_error)? {
        let path = entry.map_err(read_error)?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if name.ends_with(BACKUP_SUFFIX) || name.ends_with(TEMP_SUFFIX) || !keep(&name) || !path.is_file() {
            continue;
        }
        fs::copy(&path, to.join(&name)).map_err(|e| AppError::io(format!("複製 {}", path.display()), e))?;
    }
    Ok(())
}

/// FFXIV 插件 `cboRegion` 的選項文字（插件 3.0.3.3）。
fn region_text(region: Region) -> &'static str {
    match region {
        Region::Global => "Global",
        Region::Tw => "繁體中文",
    }
}

fn region_from_text(text: &str) -> Option<Region> {
    [Region::Global, Region::Tw].into_iter().find(|&r| region_text(r) == text.trim())
}

fn other_region(region: Region) -> Region {
    match region {
        Region::Global => Region::Tw,
        Region::Tw => Region::Global,
    }
}

// ---------- 設定檔原文（ACT 與 FFXIV 插件的設定檔由 .NET 寫出：UTF-8，常帶 BOM、CRLF） ----------

/// 設定檔原文；寫回時保留 BOM。
struct XmlFile {
    bom: bool,
    text: String,
}

impl XmlFile {
    fn decode(bytes: &[u8], path: &Path) -> AppResult<Self> {
        let (bom, body) = match bytes.strip_prefix(BOM) {
            Some(rest) => (true, rest),
            None => (false, bytes),
        };
        let text = std::str::from_utf8(body)
            .map_err(|_| setup_error(format!("{} 不是 UTF-8 文字檔", path.display())))?;
        Ok(Self {
            bom,
            text: text.to_string(),
        })
    }

    fn encode(&self, text: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(BOM.len() + text.len());
        if self.bom {
            out.extend_from_slice(BOM);
        }
        out.extend_from_slice(text.as_bytes());
        out
    }
}

/// 一個開始標籤（`<name …>` 或 `<name … />`）在原文中的位置與屬性。
struct Tag {
    name: String,
    /// `<` 到 `>`（含）。
    span: Range<usize>,
    self_closing: bool,
    attrs: Vec<Attr>,
}

struct Attr {
    name: String,
    /// 引號內的範圍（不含引號）。
    value: Range<usize>,
    quote: char,
    /// 解碼實體後的值。
    text: String,
}

impl Tag {
    fn attr(&self, name: &str) -> Option<&Attr> {
        self.attrs.iter().find(|a| a.name == name)
    }
}

/// 依序列出所有開始標籤；略過註解、CDATA、處理指令、DOCTYPE 與結束標籤。遇到不成形的標籤即停止。
fn start_tags(xml: &str) -> Vec<Tag> {
    let mut tags = Vec::new();
    let mut pos = 0;
    while let Some(offset) = xml[pos..].find('<') {
        let start = pos + offset;
        let rest = &xml[start..];
        let skip_past = |end: &str| rest.find(end).map(|i| start + i + end.len());
        let next = if rest.starts_with("<!--") {
            skip_past("-->")
        } else if rest.starts_with("<![CDATA[") {
            skip_past("]]>")
        } else if rest.starts_with("<?") {
            skip_past("?>")
        } else if rest.starts_with("</") || rest.starts_with("<!") {
            skip_past(">")
        } else {
            parse_tag(xml, start).map(|tag| {
                let end = tag.span.end;
                tags.push(tag);
                end
            })
        };
        match next {
            Some(next) => pos = next,
            None => break,
        }
    }
    tags
}

fn parse_tag(xml: &str, start: usize) -> Option<Tag> {
    let bytes = xml.as_bytes();
    let name_start = start + 1;
    let name_end = name_start + xml[name_start..].find(|c: char| c.is_ascii_whitespace() || c == '/' || c == '>')?;
    if name_end == name_start {
        return None;
    }
    let name = xml[name_start..name_end].to_string();
    let mut attrs = Vec::new();
    let mut i = name_end;
    loop {
        while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        match bytes.get(i)? {
            b'>' => {
                return Some(Tag {
                    name,
                    span: start..i + 1,
                    self_closing: false,
                    attrs,
                });
            }
            b'/' => {
                return (bytes.get(i + 1) == Some(&b'>')).then(|| Tag {
                    name,
                    span: start..i + 2,
                    self_closing: true,
                    attrs,
                });
            }
            _ => {}
        }
        let eq = i + xml[i..].find('=')?;
        let attr_name = xml[i..eq].trim();
        if attr_name.is_empty() || attr_name.contains(|c: char| c.is_ascii_whitespace() || matches!(c, '<' | '>' | '/')) {
            return None;
        }
        let mut j = eq + 1;
        while bytes.get(j).is_some_and(u8::is_ascii_whitespace) {
            j += 1;
        }
        let quote = match bytes.get(j)? {
            b'"' => '"',
            b'\'' => '\'',
            _ => return None,
        };
        let value_start = j + 1;
        let value_end = value_start + xml[value_start..].find(quote)?;
        attrs.push(Attr {
            name: attr_name.to_string(),
            value: value_start..value_end,
            quote,
            text: decode_entities(&xml[value_start..value_end]),
        });
        i = value_end + 1;
    }
}

/// `Name` 屬性為 `name` 的設定項（.NET `SettingsSerializer` 寫出的 `<ComboBox Name="…" Value="…" />` 等）。
fn setting<'a>(tags: &'a [Tag], name: &str) -> Option<&'a Tag> {
    tags.iter().find(|t| t.attr("Name").is_some_and(|a| a.text == name))
}

/// 以（範圍, 新內容）改寫原文；範圍不得重疊。
fn apply_edits(xml: &str, mut edits: Vec<(Range<usize>, String)>) -> String {
    edits.sort_by_key(|(range, _)| range.start);
    let mut out = String::with_capacity(xml.len() + edits.iter().map(|(_, s)| s.len()).sum::<usize>());
    let mut pos = 0;
    for (range, text) in edits {
        out.push_str(&xml[pos..range.start]);
        out.push_str(&text);
        pos = range.end;
    }
    out.push_str(&xml[pos..]);
    out
}

/// 原文用的換行：有 CRLF 就用 CRLF。
fn newline(xml: &str) -> &'static str {
    if xml.contains("\r\n") { "\r\n" } else { "\n" }
}

/// `pos` 所在行從行首到 `pos` 的空白；這段不全是空白時為空字串。
fn line_indent(xml: &str, pos: usize) -> String {
    let line_start = xml[..pos].rfind('\n').map_or(0, |i| i + 1);
    let lead = &xml[line_start..pos];
    if lead.chars().all(|c| c == ' ' || c == '\t') { lead.to_string() } else { String::new() }
}

fn escape_attr(value: &str, quote: char) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if quote == '"' => out.push_str("&quot;"),
            '\'' if quote == '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// XML 的預先定義實體與數字字元參照；無法辨識的 `&` 原樣保留。
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        match tail.find(';').and_then(|semi| entity(&tail[1..semi]).map(|c| (c, semi))) {
            Some((c, semi)) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let code = match name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => name.strip_prefix('#')?.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

// ---------- 檔案 ----------

/// 讀整個檔案；不存在回 `None`。
fn read_optional(path: &Path, context: &str) -> AppResult<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(AppError::io(format!("{context}（{}）", path.display()), e)),
    }
}

fn parent(path: &Path) -> AppResult<&Path> {
    path.parent().ok_or_else(|| setup_error(format!("{} 沒有上層資料夾", path.display())))
}

fn setup_error(reason: String) -> AppError {
    AppError::ActSetup { reason }
}

fn never_opened(config: &Path) -> AppError {
    setup_error(format!("請先開過一次 ACT 再關閉（找不到設定檔 {}）", config.display()))
}

fn missing_field(file: &str, field: &str) -> AppError {
    setup_error(format!("{file} 裡找不到 {field}，沒有建立另一服的 ACT"))
}

/// 錯誤訊息前加上是哪一組的 ACT（「國際服、繁中服 ACT：…」）。
fn labelled(regions: &[Region], e: AppError) -> AppError {
    let label = regions.iter().map(Region::to_string).collect::<Vec<_>>().join("、");
    setup_error(format!("{label} ACT：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::running::test_support::FakeProcess;

    const BOM_STR: &str = "\u{feff}";

    /// 臨時資料夾裡的 %APPDATA%／%LOCALAPPDATA%；視為 ACT 的程序名稱是不會有人用的名字，本機正在跑的 ACT
    /// 不影響測試（「ACT 執行中」由 [`setup_refuses_while_act_runs`] 以替身程序測）。
    fn sandbox() -> (tempfile::TempDir, Env) {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = Env {
            appdata: dir.path().join("Roaming"),
            home: dir.path().join(r"Local\xiv-launch-manager\act"),
            act_images: HashSet::from([format!("xlm-test-act-{}.exe", uuid::Uuid::new_v4().simple())]),
        };
        (dir, env)
    }

    struct MainFixture {
        exe: PathBuf,
        config: PathBuf,
        ffxiv_config: PathBuf,
        ffxiv_dll: PathBuf,
        overlay: PathBuf,
        logs: PathBuf,
    }

    /// 主安裝 ACT：Program Files 下的執行檔（含不該複製的子資料夾）、%APPDATA% 下的設定（BOM＋CRLF，與 ACT
    /// 寫出的形式相同），桌面上的 FFXIV 插件資料夾。
    fn fake_main_act(root: &Path, env: &Env, region: &str) -> MainFixture {
        let act_dir = root.join(r"Program Files (x86)\Advanced Combat Tracker");
        fs::create_dir_all(act_dir.join("Screenshot")).expect("mkdir act");
        fs::write(act_dir.join(r"Screenshot\shot.png"), b"png").expect("write shot");
        let exe = act_dir.join(act::EXE_NAME);
        fs::write(&exe, b"MZ act").expect("write exe");
        fs::write(act_dir.join("ACTx86.exe"), b"MZ x86").expect("write x86");
        fs::write(act_dir.join("Advanced Combat Tracker.exe.config"), b"<configuration />").expect("write exe config");
        let ffxiv_dir = root.join(r"Desktop\ACT3011");
        fs::create_dir_all(&ffxiv_dir).expect("mkdir ffxiv");
        let ffxiv_dll = ffxiv_dir.join(FFXIV_PLUGIN_DLL);
        fs::write(&ffxiv_dll, b"ffxiv plugin").expect("write dll");
        fs::write(ffxiv_dir.join("deucalion-1.5.0.dll"), b"deucalion").expect("write deucalion");
        fs::write(ffxiv_dir.join("readme.txt"), b"not copied").expect("write readme");
        let data = env.appdata.join("Advanced Combat Tracker");
        let config_dir = data.join(CONFIG_DIR);
        fs::create_dir_all(config_dir.join("Backup")).expect("mkdir config");
        fs::write(config_dir.join(r"Backup\Advanced Combat Tracker.20261014.xml"), b"old").expect("write backup");
        fs::write(config_dir.join("RainbowMage.OverlayPlugin.config.json"), b"{}").expect("write overlay config");
        let logs = data.join(LOG_DIR);
        let overlay = data.join(r"Plugins\OverlayPlugin\OverlayPlugin.dll");
        let config = config_dir.join("Advanced Combat Tracker.config.xml");
        fs::write(&config, act_config_text(&[(true, &overlay), (true, &ffxiv_dll)], &logs)).expect("write config");
        let ffxiv_config = config_dir.join(FFXIV_CONFIG_FILE);
        fs::write(&ffxiv_config, ffxiv_config_text(region, &logs)).expect("write ffxiv config");
        MainFixture {
            exe,
            config,
            ffxiv_config,
            ffxiv_dll,
            overlay,
            logs,
        }
    }

    fn act_config_text(plugins: &[(bool, &Path)], logs: &Path) -> String {
        let entries: String = plugins
            .iter()
            .map(|(enabled, path)| {
                let enabled = if *enabled { "True" } else { "False" };
                format!("        <Plugin Enabled=\"{enabled}\" Path=\"{}\" />\r\n", path.display())
            })
            .collect();
        format!(
            "{BOM_STR}<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n<Config>\r\n    <SettingsSerializer>\r\n        \
             <String Name=\"logFilePath\" Value=\"{logs}\\Network_30303_20261014.log\" />\r\n        \
             <DirectoryInfo Name=\"folderExports\" Value=\"C:\\Users\\User\\Desktop\" />\r\n        \
             <DirectoryInfo Name=\"folderLogs\" Value=\"{logs}\" />\r\n    </SettingsSerializer>\r\n    <XmlSubs />\r\n    \
             <ActPlugins>\r\n{entries}    </ActPlugins>\r\n</Config>",
            logs = logs.display()
        )
    }

    fn ffxiv_config_text(region: &str, logs: &Path) -> String {
        format!(
            "{BOM_STR}<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\r\n<Config>\r\n\t<SettingsSerializer>\r\n\t\t\
             <ComboBox Name=\"cboLanguage\" Value=\"繁體中文\" />\r\n\t\t\
             <TextBox Name=\"txtLogFileDirectory\" Value=\"{}\" />\r\n\t\t\
             <ComboBox Name=\"cboRegion\" Value=\"{region}\" />\r\n\t</SettingsSerializer>\r\n</Config>",
            logs.display()
        )
    }

    fn settings(exe: &Path, launch_with_game: bool) -> ActSettings {
        ActSettings {
            path: Some(exe.to_path_buf()),
            launch_with_game,
        }
    }

    /// 主安裝 ACT 的 AppDataFolder 下 OverlayPlugin 的 CEF 與 cache；回傳 CEF 資料夾。
    fn fake_main_cef(env: &Env) -> PathBuf {
        let data = env.appdata.join("Advanced Combat Tracker");
        let cef = data.join(CEF_DIR);
        fs::create_dir_all(cef.join("x64")).expect("mkdir cef");
        fs::write(cef.join(r"x64\libcef.dll"), b"cef").expect("write cef");
        fs::create_dir_all(data.join("OverlayPluginCache")).expect("mkdir cache");
        cef
    }

    /// `link` 是指向 `target` 的 junction，經由它讀得到主 ACT 的 CEF 檔案。
    fn assert_cef_link(link: &Path, target: &Path) {
        assert!(junction::exists(link).expect("junction exists"), "{} 是 junction", link.display());
        assert!(act::same_location(&junction::get_target(link).expect("junction target"), target));
        assert_eq!(fs::read(link.join(r"x64\libcef.dll")).expect("read through link"), b"cef");
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("list")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn read_string(path: &Path) -> String {
        String::from_utf8(fs::read(path).expect("read")).expect("utf-8")
    }

    fn read_json(file: &Path) -> serde_json::Value {
        let bytes = fs::read(file).expect("read target");
        assert_eq!(bytes.first(), Some(&b'{'), "UTF-8 無 BOM");
        serde_json::from_slice(&bytes).expect("target json")
    }

    #[test]
    fn target_file_updates_only_the_launched_region() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().to_path_buf());
        let file = paths.act_target_file();
        let tw = GameProcess { pid: 4242, created: 134_363_756_617_913_646 };
        write_target(&paths, Region::Tw, &tw).expect("write tw");
        assert_eq!(
            read_json(&file),
            serde_json::json!({ "tw": { "pid": 4242, "created": 134_363_756_617_913_646u64 } })
        );

        let global = GameProcess { pid: 777, created: 134_363_756_000_000_001 };
        write_target(&paths, Region::Global, &global).expect("write global");
        assert_eq!(
            read_json(&file),
            serde_json::json!({
                "global": { "pid": 777, "created": 134_363_756_000_000_001u64 },
                "tw": { "pid": 4242, "created": 134_363_756_617_913_646u64 }
            })
        );

        // 同區服再啟動：只換那一項。
        let tw2 = GameProcess { pid: 5000, created: 134_363_757_000_000_000 };
        write_target(&paths, Region::Tw, &tw2).expect("write tw again");
        let value = read_json(&file);
        assert_eq!(value["tw"], serde_json::json!({ "pid": 5000, "created": 134_363_757_000_000_000u64 }));
        assert_eq!(value["global"]["pid"], 777);
        assert_eq!(names(file.parent().expect("state dir")), ["act_target.json"], "沒有留下暫存檔");
    }

    #[test]
    fn unreadable_target_file_is_replaced_with_this_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().to_path_buf());
        let file = paths.act_target_file();
        fs::create_dir_all(file.parent().expect("state dir")).expect("mkdir");
        let process = GameProcess { pid: 1, created: 2 };
        for bad in [
            "{not json",
            r#"{"tw":{"pid":"x","created":1}}"#,
            r#"{"tw":{"pid":1,"created":2},"region":"tw"}"#,
            r#"{"tw":{"pid":-1,"created":2}}"#,
        ] {
            fs::write(&file, bad).expect("write bad");
            write_target(&paths, Region::Global, &process).expect("overwrite");
            assert_eq!(read_json(&file), serde_json::json!({ "global": { "pid": 1, "created": 2 } }), "{bad}");
        }
    }

    fn account(id: &str, region: Region, act_follow: bool) -> Account {
        Account {
            id: id.into(),
            name: id.into(),
            region,
            username: format!("{id}@example.com"),
            otp: crate::store::OtpMode::None,
            profile_id: None,
            language: Default::default(),
            remember_password: false,
            extra_args: String::new(),
            act_follow,
        }
    }

    fn game(pid: u32, created: u64) -> GameProcess {
        GameProcess { pid, created }
    }

    /// 以 pid 判斷存活的替身（正式環境是 `dalamud::is_alive`）。
    fn alive_pids(pids: &[u32]) -> impl Fn(&GameProcess) -> bool + '_ {
        move |p| pids.contains(&p.pid)
    }

    #[test]
    fn fallback_picks_the_latest_started_candidate() {
        let ended = game(9, 9_000);
        let accounts = [account("ended", Region::Tw, true), account("a", Region::Tw, true), account("b", Region::Tw, true)];
        let running = BTreeMap::from([
            ("ended".to_string(), ended),
            ("a".to_string(), game(1, 1_000)),
            ("b".to_string(), game(2, 3_000)),
        ]);
        let next = fallback_target(&accounts, &running, Region::Tw, &ended, alive_pids(&[1, 2]));
        assert_eq!(next, Some(game(2, 3_000)));
    }

    #[test]
    fn fallback_skips_unfollowed_exited_other_region_and_ended_games() {
        // 每個被排除的遊戲都比 `a` 晚開：沒被排除就會被選中。
        let ended = game(9, 9_000);
        let accounts = [
            account("a", Region::Tw, true),
            account("unfollowed", Region::Tw, false),
            account("exited", Region::Tw, true),
            account("global", Region::Global, true),
            account("ended", Region::Tw, true),
            account("no-record", Region::Tw, true),
        ];
        let running = BTreeMap::from([
            ("a".to_string(), game(1, 1_000)),
            ("unfollowed".to_string(), game(2, 5_000)),
            ("exited".to_string(), game(3, 6_000)),
            ("global".to_string(), game(4, 7_000)),
            ("ended".to_string(), ended),
            // 已刪除的帳號留下的紀錄。
            ("deleted".to_string(), game(5, 8_000)),
        ]);
        // `ended` 也當成存活：結束的程序本身一律排除，不靠存活判斷。
        let alive = alive_pids(&[1, 2, 4, 5, 9]);
        assert_eq!(fallback_target(&accounts, &running, Region::Tw, &ended, &alive), Some(game(1, 1_000)));
    }

    #[test]
    fn fallback_is_none_without_candidates() {
        let ended = game(9, 9_000);
        let accounts = [
            account("ended", Region::Tw, true),
            account("unfollowed", Region::Tw, false),
            account("exited", Region::Tw, true),
            account("global", Region::Global, true),
        ];
        let running = BTreeMap::from([
            ("ended".to_string(), ended),
            ("unfollowed".to_string(), game(2, 5_000)),
            ("exited".to_string(), game(3, 6_000)),
            ("global".to_string(), game(4, 7_000)),
        ]);
        let alive = alive_pids(&[2, 4, 9]);
        assert_eq!(fallback_target(&accounts, &running, Region::Tw, &ended, &alive), None);
        assert_eq!(fallback_target(&[], &BTreeMap::new(), Region::Tw, &ended, &alive), None);
    }

    #[test]
    fn switch_back_does_not_overwrite_a_newer_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().to_path_buf());
        let file = paths.act_target_file();
        let (ended, newer) = (game(1, 1_000), game(2, 2_000));
        write_target(&paths, Region::Tw, &newer).expect("seed");
        let before = read_json(&file);

        let chose = std::cell::Cell::new(false);
        let replaced = replace_target(&file, Region::Tw, &ended, || {
            chose.set(true);
            Ok(Some(game(3, 3_000)))
        });
        assert_eq!(replaced.expect("cas"), None);
        assert!(!chose.get(), "目標已被換掉就不選");
        assert_eq!(read_json(&file), before);

        // pid 相同、建立時間不同（pid 被重用）也不是同一個程序。
        let reused = game(newer.pid, 1);
        assert_eq!(replace_target(&file, Region::Tw, &reused, || Ok(Some(game(3, 3_000)))).expect("cas"), None);
        assert_eq!(read_json(&file), before);
    }

    #[test]
    fn switch_back_replaces_only_the_ended_region() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().to_path_buf());
        let file = paths.act_target_file();
        let (ended, global, next) = (game(1, 1_000), game(7, 7_000), game(2, 2_000));
        write_target(&paths, Region::Tw, &ended).expect("seed tw");
        write_target(&paths, Region::Global, &global).expect("seed global");
        let before = read_json(&file);

        // 沒有可切回的遊戲：不寫。
        assert_eq!(replace_target(&file, Region::Tw, &ended, || Ok(None)).expect("cas"), None);
        assert_eq!(read_json(&file), before);

        assert_eq!(replace_target(&file, Region::Tw, &ended, || Ok(Some(next))).expect("cas"), Some(next));
        assert_eq!(
            read_json(&file),
            serde_json::json!({
                "global": { "pid": 7, "created": 7_000 },
                "tw": { "pid": 2, "created": 2_000 }
            })
        );
    }

    #[test]
    fn plugin_file_is_kept_equal_to_embedded_source() {
        let (_dir, env) = sandbox();
        let file = ensure_plugin_file(&env.home).expect("create");
        assert_eq!(file, env.home.join(PLUGIN_FILE));
        assert_eq!(fs::read(&file).expect("read"), PLUGIN_SOURCE.as_bytes());
        fs::write(&file, b"// older version").expect("write old");
        ensure_plugin_file(&env.home).expect("rewrite");
        assert_eq!(fs::read(&file).expect("read"), PLUGIN_SOURCE.as_bytes());
        assert_eq!(names(&env.home), [PLUGIN_FILE], "不留暫存檔");
    }

    #[test]
    fn enable_link_appends_plugin_last_keeps_format_and_backs_up_once() {
        let (dir, env) = sandbox();
        let main = fake_main_act(dir.path(), &env, "Global");
        let config = ActConfig {
            global: ActSettings::default(),
            tw: settings(&main.exe, true),
        };
        let original = read_string(&main.config);

        let changes = enable_link(&config, &env).expect("enable");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].regions, vec![Region::Tw]);
        assert_eq!(changes[0].change, RegisterChange::Added);
        // 只多一行，排在 FFXIV 插件之後；BOM、縮排、CRLF 與其他內容不變。
        let plugin = env.home.join(PLUGIN_FILE);
        let line = format!("        <Plugin Enabled=\"True\" Path=\"{}\" />\r\n", plugin.display());
        let at = original.find("    </ActPlugins>").expect("close tag");
        let registered = format!("{}{line}{}", &original[..at], &original[at..]);
        assert_eq!(read_string(&main.config), registered);
        assert_eq!(fs::read(&plugin).expect("plugin"), PLUGIN_SOURCE.as_bytes());
        let backup = with_suffix(&main.config, BACKUP_SUFFIX);
        assert_eq!(read_string(&backup), original);

        // 已登記：不重複加、不改檔。
        assert_eq!(enable_link(&config, &env).expect("again")[0].change, RegisterChange::Unchanged);
        assert_eq!(read_string(&main.config), registered);

        // 使用者在 ACT 取消勾選後再按：改回啟用；備份仍是第一次修改前的原檔。
        let disabled_line = line.replace(r#"Enabled="True""#, r#"Enabled="False""#);
        fs::write(&main.config, registered.replace(&line, &disabled_line)).expect("disable");
        let status_now = status(&config, &env).expect("status");
        assert_eq!(status_now.acts[0].state, LinkState::Disabled);
        assert_eq!(enable_link(&config, &env).expect("reenable")[0].change, RegisterChange::Reenabled);
        assert_eq!(read_string(&main.config), registered);
        assert_eq!(read_string(&backup), original);
        assert_eq!(status(&config, &env).expect("status").acts[0].state, LinkState::Registered);
        assert_eq!(
            names(main.config.parent().expect("config dir")),
            [
                "Advanced Combat Tracker.config.xml",
                "Advanced Combat Tracker.config.xml.xlm-backup",
                "Backup",
                "FFXIV_ACT_Plugin.config.xml",
                "RainbowMage.OverlayPlugin.config.json",
            ],
            "不留暫存檔"
        );
    }

    #[test]
    fn enable_link_needs_a_configured_and_opened_act() {
        let (dir, env) = sandbox();
        assert!(matches!(enable_link(&ActConfig::default(), &env), Err(AppError::ActNotConfigured)));
        let exe = dir.path().join(r"ACT\Advanced Combat Tracker.exe");
        fs::create_dir_all(exe.parent().expect("parent")).expect("mkdir");
        fs::write(&exe, b"MZ").expect("write exe");
        let config = ActConfig {
            global: settings(&exe, false),
            tw: settings(&exe, false),
        };
        assert_eq!(status(&config, &env).expect("status").acts[0].state, LinkState::NeverOpened);
        let err = enable_link(&config, &env).expect_err("never opened").to_string();
        assert!(err.starts_with("國際服、繁中服 ACT："), "{err}");
        assert!(err.contains("請先開過一次 ACT"), "{err}");
        assert!(!env.appdata.exists(), "沒有建立任何 ACT 設定或備份");
    }

    #[test]
    fn status_merges_shared_exe_and_reports_missing_exe() {
        let (dir, env) = sandbox();
        let main = fake_main_act(dir.path(), &env, "Global");
        let shared = ActConfig {
            global: settings(&main.exe, false),
            tw: settings(&main.exe, true),
        };
        let status_now = status(&shared, &env).expect("status");
        assert_eq!(status_now.acts.len(), 1);
        assert_eq!(status_now.acts[0].regions, vec![Region::Global, Region::Tw]);
        assert_eq!(status_now.acts[0].state, LinkState::NotRegistered);
        assert!(!status_now.acts[0].instance);
        assert!(status_now.instances.is_empty());
        let missing = ActConfig {
            global: settings(&dir.path().join("gone.exe"), false),
            tw: ActSettings::default(),
        };
        assert_eq!(status(&missing, &env).expect("status").acts[0].state, LinkState::ExeMissing);
        assert!(status(&ActConfig::default(), &env).expect("empty").acts.is_empty());
    }

    /// 雙開 ACT 存檔後，外掛清單是相對 exe 資料夾的路徑（protocols §6.1）：以雙開 ACT 自己的資料夾解析。
    #[test]
    fn ffxiv_plugin_resolves_relative_path_of_dual_instance() {
        let (_dir, env) = sandbox();
        let inst = env.instance_dir(Region::Tw);
        let exe = inst.join(act::EXE_NAME);
        let dll = inst.join(r"Plugins\FFXIV_ACT_Plugin").join(FFXIV_PLUGIN_DLL);
        fs::create_dir_all(dll.parent().expect("plugin dir")).expect("mkdir plugin");
        fs::create_dir_all(inst.join(CONFIG_DIR)).expect("mkdir config");
        fs::write(&exe, b"MZ act").expect("write exe");
        fs::write(&dll, b"ffxiv plugin").expect("write dll");
        let plugins = [
            (true, Path::new(r"..\XlmActBridge.cs")),
            (true, Path::new(r"Plugins\FFXIV_ACT_Plugin\FFXIV_ACT_Plugin.dll")),
        ];
        fs::write(
            inst.join(CONFIG_DIR).join(act::config_file_name(&exe)),
            act_config_text(&plugins, &inst.join(LOG_DIR)),
        )
        .expect("write config");
        assert!(act::is_portable(&exe));
        assert_eq!(ffxiv_plugin(&exe, &env), FfxivPlugin::Installed(dll));
    }

    #[test]
    fn ffxiv_plugin_reports_why_there_is_no_plugin_file() {
        let (dir, env) = sandbox();
        assert_eq!(ffxiv_plugin(&dir.path().join("gone.exe"), &env), FfxivPlugin::ExeMissing);
        let main = fake_main_act(dir.path(), &env, "Global");
        assert_eq!(ffxiv_plugin(&main.exe, &env), FfxivPlugin::Installed(main.ffxiv_dll.clone()));
        fs::remove_file(&main.ffxiv_dll).expect("remove dll");
        assert_eq!(ffxiv_plugin(&main.exe, &env), FfxivPlugin::NotInstalled, "登記的檔案不在");
        fs::write(&main.config, act_config_text(&[(true, &main.overlay)], &main.logs)).expect("write config");
        assert_eq!(ffxiv_plugin(&main.exe, &env), FfxivPlugin::NotInstalled, "沒有登記");
        fs::write(&main.config, "<Config />").expect("write config");
        assert_eq!(ffxiv_plugin(&main.exe, &env), FfxivPlugin::Unreadable);
        fs::remove_file(&main.config).expect("remove config");
        assert_eq!(ffxiv_plugin(&main.exe, &env), FfxivPlugin::NeverOpened);
    }

    #[test]
    fn choices_list_installed_dual_and_saved_paths_once() {
        let (dir, env) = sandbox();
        let main = fake_main_act(dir.path(), &env, "Global");
        let mut config = ActConfig {
            global: ActSettings::default(),
            tw: settings(&main.exe, false),
        };
        create_dual(&mut config, &env).expect("create dual");
        let inst_exe = env.home.join(r"ACT-TW\Advanced Combat Tracker.exe");
        let upper = |p: &Path| PathBuf::from(p.display().to_string().to_uppercase());
        // 偵測結果裡同一個執行檔出現兩次（大小寫不同）；設定裡的雙開 ACT 也是不同大小寫。
        let installed = || vec![main.exe.clone(), upper(&main.exe)];
        let custom = dir.path().join(r"Moved\Advanced Combat Tracker.exe");
        let saved = ActConfig {
            global: settings(&custom, true),
            tw: settings(&upper(&inst_exe), true),
        };
        let list = choices(&saved, &env, installed());
        let summary: Vec<_> = list
            .iter()
            .map(|c| (c.path.clone(), c.kind, c.region, c.exists, c.selected_by.clone()))
            .collect();
        assert_eq!(
            summary,
            [
                (main.exe.display().to_string(), ChoiceKind::Installed, None, true, vec![]),
                (inst_exe.display().to_string(), ChoiceKind::Dual, Some(Region::Tw), true, vec![Region::Tw]),
                (custom.display().to_string(), ChoiceKind::Custom, None, false, vec![Region::Global]),
            ]
        );
        assert_eq!(
            serde_json::to_value(&list[1]).expect("json"),
            serde_json::json!({
                "path": inst_exe.display().to_string(),
                "kind": "dual",
                "region": "tw",
                "exists": true,
                "selectedBy": ["tw"],
            })
        );

        // 設定的路徑與偵測結果只差大小寫：併入系統安裝那一項，不另列自訂路徑。
        let lower = ActConfig {
            global: settings(&PathBuf::from(main.exe.display().to_string().to_lowercase()), false),
            tw: ActSettings::default(),
        };
        let list = choices(&lower, &env, installed());
        assert_eq!(list.len(), 2);
        assert_eq!((list[0].kind, list[0].selected_by.clone()), (ChoiceKind::Installed, vec![Region::Global]));
        assert_eq!((list[1].kind, list[1].selected_by.clone()), (ChoiceKind::Dual, vec![]));
        let (_empty_dir, empty) = sandbox();
        assert!(choices(&ActConfig::default(), &empty, Vec::new()).is_empty(), "沒裝 ACT、沒設定：選單只剩固定項目");
    }

    #[test]
    fn create_dual_builds_the_other_region_and_writes_settings_back() {
        let (dir, env) = sandbox();
        let main = fake_main_act(dir.path(), &env, "Global");
        // 舊的單一設定遷移後落在繁中服那一組；主 ACT 的 FFXIV 插件 Region 是 Global。
        let mut config = ActConfig {
            global: ActSettings::default(),
            tw: settings(&main.exe, true),
        };
        let result = create_dual(&mut config, &env).expect("create");
        let inst = env.home.join("ACT-TW");
        let inst_exe = inst.join(act::EXE_NAME);
        assert_eq!((result.main_region, result.instance_region), (Region::Global, Region::Tw));
        assert!(result.created);
        assert_eq!(result.instance_exe, inst_exe.display().to_string());

        // 設定寫回：主 ACT 對應它的 Region，新 ACT 對應另一服；「隨遊戲開啟」沿用主 ACT 那一組。
        assert_eq!(config.global.path.as_deref(), Some(main.exe.as_path()));
        assert!(config.global.launch_with_game);
        assert_eq!(config.tw.path.as_deref(), Some(inst_exe.as_path()));
        assert!(config.tw.launch_with_game);

        // 副本內容：主 ACT 的頂層檔案、設定資料夾頂層檔案（不含 Backup）、FFXIV 插件與 deucalion、空的 log 資料夾；
        // 主 ACT 沒有 OverlayPluginCef，所以沒有建連結。
        assert_eq!(
            names(&inst),
            ["ACTx86.exe", "Advanced Combat Tracker.exe", "Advanced Combat Tracker.exe.config", "Config", "FFXIVLogs", "Plugins"]
        );
        assert_eq!(fs::read(&inst_exe).expect("exe"), b"MZ act");
        assert_eq!(
            names(&inst.join(CONFIG_DIR)),
            ["Advanced Combat Tracker.config.xml", "FFXIV_ACT_Plugin.config.xml", "RainbowMage.OverlayPlugin.config.json"]
        );
        let plugin_dir = inst.join(r"Plugins\FFXIV_ACT_Plugin");
        assert_eq!(names(&plugin_dir), [FFXIV_PLUGIN_DLL, "deucalion-1.5.0.dll"]);
        assert_eq!(fs::read(plugin_dir.join(FFXIV_PLUGIN_DLL)).expect("dll"), b"ffxiv plugin");
        assert!(names(&inst.join(LOG_DIR)).is_empty());
        assert!(act::is_portable(&inst_exe), "新 ACT 以 -portable 開啟");

        // 副本設定：FFXIV 插件指向副本、log 改到新資料夾、外掛登記在最後；Region 是另一服。
        let logs = inst.join(LOG_DIR);
        let plugin = env.home.join(PLUGIN_FILE);
        let inst_dll = plugin_dir.join(FFXIV_PLUGIN_DLL);
        assert_eq!(
            read_string(&inst.join(r"Config\Advanced Combat Tracker.config.xml")),
            act_config_text(&[(true, &main.overlay), (true, &inst_dll), (true, &plugin)], &logs)
        );
        assert_eq!(
            read_string(&inst.join(r"Config\FFXIV_ACT_Plugin.config.xml")),
            ffxiv_config_text("繁體中文", &logs)
        );
        // 主 ACT 也登記了外掛，其他不變。
        assert_eq!(
            read_string(&main.config),
            act_config_text(&[(true, &main.overlay), (true, &main.ffxiv_dll), (true, &plugin)], &main.logs)
        );
        assert_eq!(read_string(&main.ffxiv_config), ffxiv_config_text("Global", &main.logs));
        assert_eq!(names(&env.home), ["ACT-TW", PLUGIN_FILE], "不留暫存資料夾");

        let status_now = status(&config, &env).expect("status");
        assert_eq!(status_now.acts.len(), 2);
        assert!(status_now.acts.iter().all(|a| a.state == LinkState::Registered));
        assert!(!status_now.acts[0].instance && status_now.acts[1].instance);
        assert_eq!(status_now.instances.len(), 1);
        assert_eq!(status_now.instances[0].region, Region::Tw);
        assert!(status_now.instances[0].in_use);

        // 再按一次：已存在，不重建，設定照樣寫回。
        fs::write(inst.join("marker"), b"keep").expect("marker");
        config.tw = ActSettings::default();
        let again = create_dual(&mut config, &env).expect("again");
        assert!(!again.created);
        assert!(inst.join("marker").exists());
        assert_eq!(config.tw.path.as_deref(), Some(inst_exe.as_path()));

        // ACT 存檔後可攜式的外掛路徑變成相對路徑；使用者取消勾選後，狀態是「已停用」，啟用只改 Enabled。
        let inst_config = inst.join(r"Config\Advanced Combat Tracker.config.xml");
        let relative = act_config_text(
            &[
                (true, Path::new(r"Plugins\FFXIV_ACT_Plugin\FFXIV_ACT_Plugin.dll")),
                (false, Path::new(r"..\XlmActBridge.cs")),
            ],
            &logs,
        );
        fs::write(&inst_config, &relative).expect("write relative");
        assert_eq!(status(&config, &env).expect("status").acts[1].state, LinkState::Disabled);
        let changes = enable_link(&config, &env).expect("enable");
        assert_eq!(changes[0].change, RegisterChange::Unchanged);
        assert_eq!(changes[1].change, RegisterChange::Reenabled);
        assert_eq!(
            read_string(&inst_config),
            relative.replace(r#"Enabled="False" Path="..\XlmActBridge.cs""#, r#"Enabled="True" Path="..\XlmActBridge.cs""#)
        );
    }

    #[test]
    fn create_dual_failure_leaves_no_half_built_act() {
        let (dir, env) = sandbox();
        let main = fake_main_act(dir.path(), &env, "繁體中文");
        fs::write(
            &main.ffxiv_config,
            format!("{BOM_STR}<Config>\r\n\t<SettingsSerializer>\r\n\t\t<ComboBox Name=\"cboRegion\" Value=\"繁體中文\" />\r\n\t</SettingsSerializer>\r\n</Config>"),
        )
        .expect("write ffxiv config");
        let cef = fake_main_cef(&env);
        let before = fs::read(&main.config).expect("read config");
        let mut config = ActConfig {
            global: settings(&main.exe, false),
            tw: ActSettings::default(),
        };
        let err = create_dual(&mut config, &env).expect_err("missing field").to_string();
        assert!(err.contains("txtLogFileDirectory"), "{err}");
        assert_eq!(names(&env.home), [PLUGIN_FILE], "ACT-Global 與暫存資料夾都不在");
        assert_eq!(fs::read(&main.config).expect("read config"), before, "主 ACT 沒有被改");
        assert_eq!(config.global.path.as_deref(), Some(main.exe.as_path()));
        assert!(config.tw.path.is_none(), "設定沒有寫回");
        assert_eq!(
            fs::read(cef.join(r"x64\libcef.dll")).expect("main cef"),
            b"cef",
            "清除暫存資料夾時沒有跟著 junction 刪到主 ACT 的 CEF"
        );
    }

    #[test]
    fn create_dual_refuses_read_only_main_config() {
        let (dir, env) = sandbox();
        let main = fake_main_act(dir.path(), &env, "繁體中文");
        let config_dir = main.config.parent().expect("config dir").to_path_buf();
        let before = fs::read(&main.config).expect("read config");
        let files_before = names(&config_dir);
        // 唯讀屬性模擬目前權限寫不進去的設定檔；結束前還原原本的屬性，臨時資料夾才刪得掉。
        let original = fs::metadata(&main.config).expect("metadata").permissions();
        let mut read_only = original.clone();
        read_only.set_readonly(true);
        fs::set_permissions(&main.config, read_only).expect("set read-only");
        let mut config = ActConfig {
            global: ActSettings::default(),
            tw: settings(&main.exe, true),
        };
        let result = create_dual(&mut config, &env);
        fs::set_permissions(&main.config, original).expect("restore permissions");

        let err = result.expect_err("read-only main config").to_string();
        assert!(err.contains("目前的權限無法寫入主 ACT 的設定檔"), "{err}");
        assert!(!env.home.exists(), "沒有寫外掛檔、沒有建立雙開 ACT");
        assert_eq!(fs::read(&main.config).expect("read config"), before, "主 ACT 沒有被改");
        assert_eq!(names(&config_dir), files_before, "沒有留下探測檔或備份");
        assert!(config.global.path.is_none(), "設定沒有寫回");
    }

    #[test]
    fn create_dual_shares_main_overlay_cef_through_junction() {
        let (dir, env) = sandbox();
        let main = fake_main_act(dir.path(), &env, "繁體中文");
        let cef = fake_main_cef(&env);
        let mut config = ActConfig {
            global: settings(&main.exe, true),
            tw: ActSettings::default(),
        };
        assert!(create_dual(&mut config, &env).expect("create").created);
        let inst = env.home.join("ACT-Global");
        let link = inst.join(CEF_DIR);
        assert_cef_link(&link, &cef);
        assert!(!inst.join("OverlayPluginCache").exists(), "CEF 的 cache 不共用");

        // 已存在的雙開 ACT 沒有 OverlayPluginCef（例如在這個功能之前建的）：再按一次就補建。
        fs::remove_dir(&link).expect("remove link");
        assert!(cef.join(r"x64\libcef.dll").is_file(), "刪連結不影響主 ACT 的 CEF");
        assert!(!create_dual(&mut config, &env).expect("again").created);
        assert_cef_link(&link, &cef);

        // 既有的 junction（指向別處）不動。
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir(&elsewhere).expect("mkdir elsewhere");
        fs::remove_dir(&link).expect("remove link");
        junction::create(&elsewhere, &link).expect("junction elsewhere");
        create_dual(&mut config, &env).expect("again");
        assert!(act::same_location(&junction::get_target(&link).expect("target"), &elsewhere));

        // 使用者自己讓 OverlayPlugin 下載的實體資料夾不動。
        fs::remove_dir(&link).expect("remove link");
        fs::create_dir_all(link.join("x64")).expect("mkdir own cef");
        fs::write(link.join(r"x64\libcef.dll"), b"own").expect("write own cef");
        create_dual(&mut config, &env).expect("again");
        assert!(!fs::symlink_metadata(&link).expect("metadata").file_type().is_symlink(), "仍是實體資料夾");
        assert_eq!(fs::read(link.join(r"x64\libcef.dll")).expect("own cef"), b"own");
    }

    /// 刪除雙開 ACT 資料夾用的 std `remove_dir_all`（失敗清除）只刪 junction 本身，不進入主 ACT 的 CEF。
    #[test]
    fn instance_cleanup_keeps_main_cef() {
        let (dir, env) = sandbox();
        let fixture = fake_main_act(dir.path(), &env, "Global");
        let cef = fake_main_cef(&env);
        let main = MainAct::read(fixture.exe, &env).expect("main act");
        let inst = dir.path().join("instance");
        fs::create_dir(&inst).expect("mkdir instance");
        link_cef(&main, &inst).expect("link");
        assert_cef_link(&inst.join(CEF_DIR), &cef);
        fs::remove_dir_all(&inst).expect("remove instance");
        assert!(!inst.exists());
        assert_eq!(names(&cef), ["x64"]);
        assert_eq!(fs::read(cef.join(r"x64\libcef.dll")).expect("main cef"), b"cef");
    }

    #[test]
    fn create_dual_needs_region_and_ffxiv_plugin() {
        let (dir, env) = sandbox();
        let main = fake_main_act(dir.path(), &env, "Korean");
        let mut config = ActConfig {
            global: settings(&main.exe, false),
            tw: ActSettings::default(),
        };
        let region_err = |config: &mut ActConfig| create_dual(config, &env).expect_err("refused").to_string();
        assert!(region_err(&mut config).contains("選好 Region"));
        fs::remove_file(&main.ffxiv_config).expect("remove ffxiv config");
        assert!(region_err(&mut config).contains("選好 Region"), "FFXIV 插件從沒存過設定");
        fs::write(&main.ffxiv_config, ffxiv_config_text("Global", &main.logs)).expect("write ffxiv config");
        fs::write(&main.config, act_config_text(&[(true, &main.overlay)], &main.logs)).expect("write config");
        assert!(region_err(&mut config).contains("FFXIV 插件"));
        assert!(!env.home.join("ACT-TW").exists());
        assert!(config.tw.path.is_none());
        // 只設定了本程式建立的雙開 ACT：沒有主 ACT。
        let only_instance = ActConfig {
            global: ActSettings::default(),
            tw: settings(&env.home.join(r"ACT-TW\Advanced Combat Tracker.exe"), false),
        };
        assert!(matches!(create_dual(&mut only_instance.clone(), &env), Err(AppError::ActNotConfigured)));
    }

    #[test]
    fn setup_refuses_while_act_runs() {
        let (dir, env) = sandbox();
        let main = fake_main_act(dir.path(), &env, "Global");
        let image = env.act_images.iter().next().expect("image").clone();
        let _act = FakeProcess::start(&dir.path().join("running").join(image));
        let mut config = ActConfig {
            global: ActSettings::default(),
            tw: settings(&main.exe, false),
        };
        let before = fs::read(&main.config).expect("read config");
        assert!(matches!(enable_link(&config, &env), Err(AppError::ActRunning)));
        assert!(matches!(create_dual(&mut config, &env), Err(AppError::ActRunning)));
        assert_eq!(fs::read(&main.config).expect("read config"), before);
        assert!(!env.home.exists(), "沒有寫外掛檔、沒有建立雙開 ACT");
        assert!(config.global.path.is_none());
    }
}
