//! 「最後由 <帳號> 使用」標記：本程式開的遊戲結束時，該區服在這次遊戲執行期間被改過的角色
//! 恰好一個，才把它標成這個帳號。同區服另有客戶端（開始或結束時掃描到、或本程式開的同區服遊戲時間重疊）時不標。
//! 標記存在 `state/character_usage.json`，只供畫面輔助辨認。

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::dalamud::{self, GameProcess};
use crate::error::{AppError, AppResult};
use crate::patch::running;
use crate::region::Region;
use crate::store::{self, AppPaths};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub account_id: String,
    /// 標記時間（Unix 秒）。
    pub at: u64,
}

/// 區服 → 角色 id → 標記。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct UsageFile {
    #[serde(default)]
    pub global: BTreeMap<String, Usage>,
    #[serde(default)]
    pub tw: BTreeMap<String, Usage>,
}

impl UsageFile {
    pub fn get(&self, region: Region) -> &BTreeMap<String, Usage> {
        match region {
            Region::Global => &self.global,
            Region::Tw => &self.tw,
        }
    }

    fn get_mut(&mut self, region: Region) -> &mut BTreeMap<String, Usage> {
        match region {
            Region::Global => &mut self.global,
            Region::Tw => &mut self.tw,
        }
    }
}

/// 標記檔的讀改寫鎖；臨界區只有讀檔、改 map、原子寫檔，panic 不會留下半更新的狀態，poison 後可直接沿用。
static FILE: Mutex<()> = Mutex::new(());

fn file_lock() -> MutexGuard<'static, ()> {
    FILE.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn load(paths: &AppPaths) -> AppResult<UsageFile> {
    let _guard = file_lock();
    read(&paths.character_usage_file())
}

/// 檔案不存在＝沒有標記；格式錯誤回報錯誤（不覆寫）。
fn read(file: &Path) -> AppResult<UsageFile> {
    match fs::read(file) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| AppError::Io {
            context: "解析 character_usage.json".into(),
            message: e.to_string(),
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(UsageFile::default()),
        Err(e) => Err(AppError::io("讀取 character_usage.json", e)),
    }
}

fn record(paths: &AppPaths, region: Region, id: &str, usage: Usage) -> AppResult<()> {
    let _guard = file_lock();
    let file = paths.character_usage_file();
    let mut all = read(&file)?;
    all.get_mut(region).insert(id.to_string(), usage);
    store::write_json_atomic(&file, &all)
}

/// `GetProcessTimes` 的 FILETIME（1601-01-01 起的 100ns）→ `SystemTime`。
fn filetime_to_system(filetime: u64) -> SystemTime {
    const UNIX_EPOCH_AS_FILETIME: u64 = 116_444_736_000_000_000;
    UNIX_EPOCH + Duration::from_nanos(filetime.saturating_sub(UNIX_EPOCH_AS_FILETIME).saturating_mul(100))
}

/// 最後遊玩時間落在 `[start, end]` 的角色恰好一個時回它；零個或多個回 `None`。
fn played_character(characters: &[(String, Option<SystemTime>)], start: SystemTime, end: SystemTime) -> Option<&str> {
    let mut hits = characters
        .iter()
        .filter(|(_, played)| played.is_some_and(|t| start <= t && t <= end));
    match (hits.next(), hits.next()) {
        (Some((id, _)), None) => Some(id),
        _ => None,
    }
}

/// 除了 `own_pid` 之外，是否有同區服的遊戲客戶端（protocols §3 角色資料例外的判定）。
fn others_running(other_region_exes: &[PathBuf], own_pid: u32) -> AppResult<bool> {
    Ok(running::game_clients(other_region_exes)?.into_iter().any(|pid| pid != own_pid))
}

fn log_error(region: Region, what: &str, e: &AppError) {
    eprintln!("{region} 角色使用標記：{what}：{e}");
}

/// 本程式開的遊戲的等待者；同一份狀態記著執行中的遊戲，判斷本程式開的同區服遊戲是否重疊。
#[derive(Clone)]
pub struct UsageWatch(Arc<Shared>);

struct Shared {
    paths: AppPaths,
    sessions: Mutex<Vec<Session>>,
}

struct Session {
    region: Region,
    process: GameProcess,
    /// 期間有其他同區服客戶端：結束時不標。
    ambiguous: bool,
}

impl UsageWatch {
    pub fn new(paths: AppPaths) -> Self {
        Self(Arc::new(Shared {
            paths,
            sessions: Mutex::new(Vec::new()),
        }))
    }

    fn sessions(&self) -> MutexGuard<'_, Vec<Session>> {
        // 臨界區只增刪、改旗標，panic 不會留下半更新的狀態，poison 後可直接沿用。
        self.0.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 遊戲開出後呼叫：記下開始時的狀態，開一條執行緒等它結束後判定並標記（錯誤只記 log）。
    /// `other_region_exes`：其他區服設定的遊戲執行檔（同區服客戶端判定時排除）。
    pub fn follow(&self, region: Region, account_id: &str, process: GameProcess, other_region_exes: Vec<PathBuf>) {
        let others_at_start = others_running(&other_region_exes, process.pid).unwrap_or_else(|e| {
            log_error(region, "無法確認同區服是否另有客戶端，這次遊戲不標", &e);
            true
        });
        self.begin(region, process, others_at_start);
        let this = self.clone();
        let account_id = account_id.to_string();
        let spawned = thread::Builder::new().name("character-usage-watch".into()).spawn(move || {
            match dalamud::wait_for_exit(&process) {
                Ok(()) => this.finish(region, &account_id, &process, &other_region_exes),
                Err(e) => {
                    this.take(&process);
                    log_error(region, "無法等待遊戲結束，這次遊戲不標", &e);
                }
            }
        });
        if let Err(e) = spawned {
            self.take(&process);
            log_error(region, "無法等待遊戲結束，這次遊戲不標", &AppError::io("建立執行緒", e));
        }
    }

    /// 記下開始的遊戲：開始時已有同區服客戶端（`others_at_start`），或本程式開的同區服遊戲仍在執行時，兩邊都不標。
    fn begin(&self, region: Region, process: GameProcess, others_at_start: bool) {
        let mut sessions = self.sessions();
        let mut ambiguous = others_at_start;
        for session in sessions.iter_mut().filter(|s| s.region == region) {
            session.ambiguous = true;
            ambiguous = true;
        }
        sessions.push(Session { region, process, ambiguous });
    }

    /// 移除這個遊戲的紀錄，回它是否不標。
    fn take(&self, process: &GameProcess) -> bool {
        let mut sessions = self.sessions();
        match sessions.iter().position(|s| s.process == *process) {
            Some(index) => sessions.remove(index).ambiguous,
            None => true,
        }
    }

    fn finish(&self, region: Region, account_id: &str, process: &GameProcess, other_region_exes: &[PathBuf]) {
        if self.take(process) {
            return;
        }
        match others_running(other_region_exes, process.pid) {
            Ok(false) => {}
            Ok(true) => return,
            Err(e) => return log_error(region, "無法確認同區服是否另有客戶端，這次遊戲不標", &e),
        }
        let end = SystemTime::now();
        let start = filetime_to_system(process.created);
        let marked = (|| {
            let game_dir = crate::game::config_dir(region)?;
            let mut characters = Vec::new();
            for (id, dir) in super::character_dirs(&game_dir)? {
                characters.push((id, super::last_played(&dir)?));
            }
            match played_character(&characters, start, end) {
                Some(id) => record(
                    &self.0.paths,
                    region,
                    id,
                    Usage {
                        account_id: account_id.to_string(),
                        at: super::unix_secs(end),
                    },
                ),
                None => Ok(()),
            }
        })();
        if let Err(e) = marked {
            log_error(region, "標記失敗", &e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn only_a_single_character_played_in_the_session_is_marked() {
        let characters = vec![
            ("A".to_string(), Some(at(150))),
            ("B".to_string(), Some(at(90))),
            ("C".to_string(), None),
        ];
        assert_eq!(played_character(&characters, at(100), at(200)), Some("A"));
        // 端點算在內。
        assert_eq!(played_character(&characters, at(150), at(150)), Some("A"));
        // 沒有角色在區間內被改。
        assert_eq!(played_character(&characters, at(160), at(200)), None);
        // 兩個角色在區間內被改：無法判定是哪一個。
        assert_eq!(played_character(&characters, at(80), at(200)), None);
    }

    #[test]
    fn filetime_converts_to_unix_time() {
        // 2026-10-21T00:00:00Z＝Unix 1792540800。
        let filetime = (1_792_540_800 + 11_644_473_600) * 10_000_000;
        assert_eq!(filetime_to_system(filetime), at(1_792_540_800));
    }

    #[test]
    fn records_round_trip_per_region() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().to_path_buf());
        assert!(load(&paths).expect("empty").global.is_empty());
        let usage = Usage { account_id: "a1".into(), at: 5 };
        record(&paths, Region::Tw, "004000028FABD965", usage.clone()).expect("record");
        let file = load(&paths).expect("load");
        assert_eq!(file.get(Region::Tw).get("004000028FABD965"), Some(&usage));
        assert!(file.get(Region::Global).is_empty());
    }

    #[test]
    fn overlapping_sessions_in_this_program_are_not_marked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let watch = UsageWatch::new(AppPaths::new(dir.path().to_path_buf()));
        let first = GameProcess { pid: 1, created: 1 };
        let second = GameProcess { pid: 2, created: 2 };
        let other_region = GameProcess { pid: 3, created: 3 };
        let alone = GameProcess { pid: 4, created: 4 };
        watch.begin(Region::Tw, first, false);
        watch.begin(Region::Global, other_region, false);
        // 後開的同區服遊戲把仍在執行的一起標成不標；另一區服不受影響。
        watch.begin(Region::Tw, second, false);
        assert!(watch.take(&first));
        assert!(watch.take(&second));
        assert!(!watch.take(&other_region));
        // 前一個已結束（已移除）後才開的不受影響；開始時掃到其他客戶端則不標。
        watch.begin(Region::Tw, alone, false);
        assert!(!watch.take(&alone));
        watch.begin(Region::Tw, alone, true);
        assert!(watch.take(&alone));
        // 不是本程式開的（沒有紀錄）視為不標。
        assert!(watch.take(&first));
    }
}
