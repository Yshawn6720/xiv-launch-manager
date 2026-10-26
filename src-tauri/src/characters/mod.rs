//! 角色資料（protocols §7、主 note `docs/specs/2026-10-21-character-data.md`）：列出、備份、還原、轉移。
//! 寫入互斥由呼叫端在取得 app-wide 鎖之後、第一筆寫入之前以 [`ensure_writable`] 檢查。
//!
//! 備份：`<備份根>\FFXIV_CHR<id>\<備份 id>\files\`（角色資料夾頂層的一般檔案）與 `shared\`（區服資料夾的共用檔，
//! 只供使用者手動取用，本程式不寫回）。寫入（還原、轉移）前先自動備份目標角色；寫到一半失敗時以這份自動備份
//! 把已寫的檔恢復、刪掉這次新建的檔。

pub mod commands;
pub mod format;
pub mod usage;

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::patch::running;
use crate::region::Region;
use crate::store::{self, Config, TEMP_SUFFIX};

/// 角色資料夾頂層的遊戲檔（protocols §7.1）；「最後遊玩時間」只看這些主檔。
pub const GAME_FILES: [&str; 14] = [
    "ACQ.DAT",
    "ADDON.DAT",
    "COMMON.DAT",
    "CONTROL0.DAT",
    "CONTROL1.DAT",
    "GEARSET.DAT",
    "GS.DAT",
    "HOTBAR.DAT",
    "ITEMFDR.DAT",
    "ITEMODR.DAT",
    "KEYBIND.DAT",
    "LOGFLTR.DAT",
    "MACRO.DAT",
    "UISAVE.DAT",
];
/// 區服資料夾的共用檔：一起備份，不寫回（寫回會改到同區服的其他角色）。
const SHARED_FILES: [&str; 2] = ["FFXIV.cfg", "MACROSYS.dat"];
const FOLDER_PREFIX: &str = "FFXIV_CHR";
/// 每個角色保留的自動備份份數。
const AUTO_KEEP: usize = 10;

/// 角色 id＝16 位 hex（protocols §7.3）。
pub fn is_valid_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn check_id(id: &str) -> AppResult<()> {
    if is_valid_id(id) { Ok(()) } else { Err(AppError::InvalidCharacterId) }
}

fn folder_name(id: &str) -> String {
    format!("{FOLDER_PREFIX}{id}")
}

/// 角色資料夾（呼叫端已驗證 `id`）。
pub fn character_dir(game_dir: &Path, id: &str) -> PathBuf {
    game_dir.join(folder_name(id))
}

/// 這個角色的備份資料夾（呼叫端已驗證 `id`）。
pub fn backups_of(backups_dir: &Path, id: &str) -> PathBuf {
    backups_dir.join(folder_name(id))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BackupKind {
    Manual,
    Auto,
}

impl BackupKind {
    fn suffix(self) -> &'static str {
        match self {
            BackupKind::Manual => "manual",
            BackupKind::Auto => "auto",
        }
    }
}

/// 備份 id＝`YYYYMMDD-HHMMSS-mmm-<manual|auto>`（本機時間）；不符回 `None`。字串順序即時間順序。
pub fn parse_backup_id(id: &str) -> Option<BackupKind> {
    let (stamp, kind) = id.rsplit_once('-')?;
    let kind = match kind {
        "manual" => BackupKind::Manual,
        "auto" => BackupKind::Auto,
        _ => return None,
    };
    let shape = stamp.len() == 19
        && stamp.bytes().enumerate().all(|(i, b)| if i == 8 || i == 15 { b == b'-' } else { b.is_ascii_digit() });
    shape.then_some(kind)
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BackupInfo {
    pub id: String,
    pub kind: BackupKind,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CharacterInfo {
    pub id: String,
    /// 最後遊玩時間（Unix 秒）：[`GAME_FILES`] 主檔的最大修改時間；一個都沒有時為 `None`。
    pub last_played: Option<u64>,
    /// 新到舊。
    pub backups: Vec<BackupInfo>,
}

/// `game_dir`（區服資料夾）下的角色，依最後遊玩時間新到舊；`game_dir` 不存在回空清單。
pub fn list(game_dir: &Path, backups_dir: &Path) -> AppResult<Vec<CharacterInfo>> {
    let mut out = Vec::new();
    for (id, dir) in character_dirs(game_dir)? {
        out.push(CharacterInfo {
            last_played: last_played(&dir)?.map(unix_secs),
            backups: backups(backups_dir, &id)?,
            id,
        });
    }
    out.sort_by(|a, b| b.last_played.cmp(&a.last_played).then_with(|| a.id.cmp(&b.id)));
    Ok(out)
}

/// 名稱完全符合 `FFXIV_CHR` 加 16 位 hex 的資料夾：(id, 路徑)。
pub(crate) fn character_dirs(game_dir: &Path) -> AppResult<Vec<(String, PathBuf)>> {
    let entries = match fs::read_dir(game_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(AppError::io("讀取遊戲設定資料夾", e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| AppError::io("讀取遊戲設定資料夾", e))?;
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_prefix(FOLDER_PREFIX)) else {
            continue;
        };
        let is_dir = entry.file_type().map_err(|e| AppError::io("讀取遊戲設定資料夾", e))?.is_dir();
        if is_valid_id(id) && is_dir {
            out.push((id.to_string(), entry.path()));
        }
    }
    Ok(out)
}

/// [`GAME_FILES`] 主檔的最大修改時間（不含 `.old`、`.bak` 等其他檔）。
pub(crate) fn last_played(char_dir: &Path) -> AppResult<Option<SystemTime>> {
    let mut latest = None;
    for name in GAME_FILES {
        let modified = match fs::metadata(char_dir.join(name)) {
            Ok(meta) => meta.modified().map_err(|e| AppError::io(format!("讀取 {name} 修改時間"), e))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(AppError::io(format!("讀取 {name}"), e)),
        };
        latest = latest.max(Some(modified));
    }
    Ok(latest)
}

pub(crate) fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// 角色的備份，新到舊；名稱不符備份 id 格式的資料夾（含建立中的暫存資料夾）略過。
pub fn backups(backups_dir: &Path, id: &str) -> AppResult<Vec<BackupInfo>> {
    let entries = match fs::read_dir(backups_of(backups_dir, id)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(AppError::io("讀取角色備份資料夾", e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| AppError::io("讀取角色備份資料夾", e))?;
        if let Some(name) = entry.file_name().to_str()
            && let Some(kind) = parse_backup_id(name)
        {
            out.push(BackupInfo { id: name.to_string(), kind });
        }
    }
    out.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(out)
}

/// 資料夾頂層的一般檔案名稱（不含子資料夾、symlink 與本程式的暫存檔）。
fn top_files(dir: &Path) -> io::Result<Vec<OsString>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if entry.file_type()?.is_file() && !name.to_string_lossy().ends_with(TEMP_SUFFIX) {
            out.push(name);
        }
    }
    Ok(out)
}

/// 讀檔與修改時間。
fn read_with_time(path: &Path) -> io::Result<(Vec<u8>, SystemTime)> {
    let bytes = fs::read(path)?;
    let modified = fs::metadata(path)?.modified()?;
    Ok((bytes, modified))
}

/// 讀檔與修改時間；不存在回 `None`。
fn read_optional(path: &Path) -> io::Result<Option<(Vec<u8>, SystemTime)>> {
    match read_with_time(path) {
        Ok(read) => Ok(Some(read)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// 複製一個檔並保留修改時間；寫完 `sync_all`（還原前的自動備份必須先落盤，才能覆寫原檔）。
fn copy_durable(from: &Path, to: &Path) -> io::Result<()> {
    let (bytes, modified) = read_with_time(from)?;
    store::write_new_at(to, &bytes, Some(modified))
}

/// 建立 `kind` 備份，回備份 id。只讀遊戲檔，不需寫入互斥。先寫到同層暫存資料夾，完成後改名；失敗時刪掉暫存資料夾。
pub fn backup(game_dir: &Path, backups_dir: &Path, id: &str, kind: BackupKind) -> AppResult<String> {
    let char_dir = character_dir(game_dir, id);
    if !char_dir.is_dir() {
        return Err(AppError::NotFound {
            what: format!("角色資料夾 {}", char_dir.display()),
        });
    }
    let parent = backups_of(backups_dir, id);
    fs::create_dir_all(&parent).map_err(|e| AppError::io("建立角色備份資料夾", e))?;
    let backup_id = format!("{}-{}", chrono::Local::now().format("%Y%m%d-%H%M%S-%3f"), kind.suffix());
    let staging = parent.join(format!(".{}{TEMP_SUFFIX}", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let files = staging.join("files");
        fs::create_dir_all(&files)?;
        for name in top_files(&char_dir)? {
            copy_durable(&char_dir.join(&name), &files.join(&name))?;
        }
        let shared = staging.join("shared");
        fs::create_dir(&shared)?;
        for name in SHARED_FILES {
            match copy_durable(&game_dir.join(name), &shared.join(name)) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                other => other?,
            }
        }
        store::rename_dir_write_through(&staging, &parent.join(&backup_id))
    })();
    if let Err(e) = result {
        // quality-allow: 暫存資料夾清理是 best-effort；名稱不符備份 id 格式，清單不會列出，錯誤照常回報
        let _ = fs::remove_dir_all(&staging);
        return Err(AppError::io("建立角色備份", e));
    }
    Ok(backup_id)
}

/// 要寫進角色資料夾頂層的一個檔。
#[derive(Debug)]
pub struct FileWrite {
    pub name: OsString,
    pub bytes: Vec<u8>,
    /// 寫入後的修改時間（還原＝備份裡的檔；轉移＝目標原本的檔，沒有時用來源的檔）。
    pub modified: SystemTime,
}

/// 把角色還原成 `backup_id`：備份 `files\` 裡的每個檔寫回角色資料夾頂層；角色資料夾裡有、備份裡沒有的檔不動。
/// 呼叫端須已通過 [`ensure_writable`]。回寫入前的自動備份 id。
pub fn restore(game_dir: &Path, backups_dir: &Path, id: &str, backup_id: &str) -> AppResult<String> {
    if parse_backup_id(backup_id).is_none() {
        return Err(AppError::InvalidCharacterId);
    }
    let source = backups_of(backups_dir, id).join(backup_id).join("files");
    if !source.is_dir() {
        return Err(AppError::NotFound {
            what: format!("角色備份 {backup_id}"),
        });
    }
    let read = || -> io::Result<Vec<FileWrite>> {
        let mut writes = Vec::new();
        for name in top_files(&source)? {
            let (bytes, modified) = read_with_time(&source.join(&name))?;
            writes.push(FileWrite { name, bytes, modified });
        }
        Ok(writes)
    };
    let writes = read().map_err(|e| AppError::io("讀取角色備份", e))?;
    apply(game_dir, backups_dir, id, &writes)
}

/// 自動備份目標角色 → 依序原子寫入 `writes` → 成功後裁剪自動備份。中途失敗時以這份自動備份恢復（[`rollback`]）。
/// 角色資料夾或要寫的檔是連結時不寫。回自動備份 id。
fn apply(game_dir: &Path, backups_dir: &Path, id: &str, writes: &[FileWrite]) -> AppResult<String> {
    let char_dir = character_dir(game_dir, id);
    // 備份不收連結；寫入會把連結換成一般檔，失敗回復時它又不在備份裡而被刪掉。
    let linked = std::iter::once(char_dir.clone())
        .chain(writes.iter().map(|w| char_dir.join(&w.name)))
        .find(|path| fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()));
    if let Some(path) = linked {
        return Err(AppError::CharacterLinked { path: path.display().to_string() });
    }
    let auto = backup(game_dir, backups_dir, id, BackupKind::Auto)?;
    for (index, write) in writes.iter().enumerate() {
        if let Err(e) = store::write_replace_modified(&char_dir.join(&write.name), &write.bytes, write.modified) {
            // 失敗的那一個檔是原子取代，沒有改到；只恢復之前已寫的。
            return Err(rollback(&char_dir, &backups_of(backups_dir, id), &auto, &writes[..index], &e));
        }
    }
    if let Err(e) = prune_auto(backups_dir, id, &auto) {
        // 寫入已成功；裁剪失敗只多留幾份舊備份，下次成功時再裁。
        eprintln!("裁剪角色 {id} 的自動備份失敗：{e}");
    }
    Ok(auto)
}

/// 把 `written` 恢復成自動備份 `backup_id` 裡的狀態：備份裡有的檔寫回（含修改時間），沒有的（這次新建的）刪掉。
/// 恢復也失敗時把這份自動備份改成手動備份（不被裁剪），錯誤指出它、改名是否成功與可能留下的新建檔。
fn rollback(char_dir: &Path, backups: &Path, backup_id: &str, written: &[FileWrite], cause: &io::Error) -> AppError {
    let saved = backups.join(backup_id).join("files");
    let mut failed = None;
    for write in written {
        let target = char_dir.join(&write.name);
        let restored = match read_optional(&saved.join(&write.name)) {
            Ok(Some((bytes, modified))) => store::write_replace_modified(&target, &bytes, modified),
            Ok(None) => match fs::remove_file(&target) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                other => other,
            },
            Err(e) => Err(e),
        };
        if let Err(e) = restored {
            failed.get_or_insert(e);
        }
    }
    let reason = cause.to_string();
    let Some(rollback_error) = failed else {
        return AppError::CharacterWriteRolledBack { reason };
    };
    // 改名前判斷：改名後 `saved` 就不在了，原有的檔會被誤判成新建檔。
    let created: Vec<String> = written
        .iter()
        .filter(|w| !saved.join(&w.name).exists() && char_dir.join(&w.name).exists())
        .map(|w| w.name.to_string_lossy().into_owned())
        .collect();
    let manual = backup_id.replace("-auto", "-manual");
    let (backup, kept) = match store::rename_dir_write_through(&backups.join(backup_id), &backups.join(&manual)) {
        Ok(()) => (manual, "已改為手動備份".to_string()),
        Err(e) => (
            backup_id.to_string(),
            format!("沒能改成手動備份（{e}），之後的還原或轉移可能把它當成舊的自動備份刪掉"),
        ),
    };
    AppError::CharacterRollbackFailed {
        reason: format!("{reason}；恢復時：{rollback_error}"),
        backup,
        kept,
        created: if created.is_empty() { String::new() } else { format!("，並刪除這次新建的 {}", created.join("、")) },
    }
}

/// 只留最新 [`AUTO_KEEP`] 份自動備份（依 id 的時間）；手動備份與這次剛建立的 `current` 不動。時鐘被調回時
/// `current` 的 id 可能比舊備份小，它是這次寫入前的狀態，不能因此被裁掉。
fn prune_auto(backups_dir: &Path, id: &str, current: &str) -> io::Result<()> {
    let list = backups(backups_dir, id).map_err(|e| io::Error::other(e.to_string()))?;
    let dir = backups_of(backups_dir, id);
    for old in list.iter().filter(|b| b.kind == BackupKind::Auto && b.id != current).skip(AUTO_KEEP - 1) {
        fs::remove_dir_all(dir.join(&old.id))?;
    }
    Ok(())
}

/// 寫入互斥（protocols §7.3）：該區服須設定遊戲路徑；該安裝的遊戲或官方啟動器、或任何不是其他區服遊戲執行檔的
/// `ffxiv_dx11.exe` 在執行就拒絕。呼叫端在取得 app-wide 鎖之後、第一筆寫入之前呼叫。
pub fn ensure_writable(config: &Config, region: Region) -> AppResult<()> {
    let install = config.games.get(region).ok_or(AppError::GameNotConfigured { region })?;
    if running::install_in_use(install)? || !running::game_clients(&other_region_exes(config, region))?.is_empty() {
        return Err(AppError::CharacterDataInUse { region });
    }
    Ok(())
}

/// 其他區服設定的遊戲執行檔（同區服遊戲客戶端判定時排除）。
pub fn other_region_exes(config: &Config, region: Region) -> Vec<PathBuf> {
    [Region::Global, Region::Tw]
        .into_iter()
        .filter(|&r| r != region)
        .filter_map(|r| config.games.get(r).map(crate::game::game_exe))
        .collect()
}

/// 轉移的分類（protocols §7.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Category {
    Hotbar,
    Macro,
    Keybind,
    ChatFilter,
    Hud,
    CharacterSettings,
    Controls,
}

pub const CATEGORIES: [Category; 7] = [
    Category::Hotbar,
    Category::Macro,
    Category::Keybind,
    Category::ChatFilter,
    Category::Hud,
    Category::CharacterSettings,
    Category::Controls,
];

impl Category {
    fn files(self) -> &'static [&'static str] {
        match self {
            Category::Hotbar => &["HOTBAR.DAT"],
            Category::Macro => &["MACRO.DAT"],
            Category::Keybind => &["KEYBIND.DAT"],
            Category::ChatFilter => &["LOGFLTR.DAT"],
            Category::Hud => &["ADDON.DAT"],
            Category::CharacterSettings => &["COMMON.DAT"],
            Category::Controls => &["CONTROL0.DAT", "CONTROL1.DAT"],
        }
    }

    fn is_text(self) -> bool {
        match self {
            Category::CharacterSettings | Category::Controls => true,
            Category::Hotbar | Category::Macro | Category::Keybind | Category::ChatFilter | Category::Hud => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Action {
    /// 整檔複製。
    Copy,
    /// 純文字以 key 合併。
    Merge,
    /// 不轉（`reason`）。
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CategoryPlan {
    pub category: Category,
    pub action: Action,
    pub reason: Option<String>,
    /// 合併時來源有、目標沒有，這次不會新增的 key 數。
    pub missing_keys: usize,
}

/// 從來源角色資料夾 `source` 轉到目標角色資料夾 `target` 的計畫與各分類要寫的檔。`same_region`：兩者同區服。
fn plan_writes(source: &Path, target: &Path, same_region: bool) -> AppResult<Vec<(CategoryPlan, Vec<FileWrite>)>> {
    let read = |path: &Path| read_optional(path).map_err(|e| AppError::io(format!("讀取 {}", path.display()), e));
    let mut out = Vec::new();
    for category in CATEGORIES {
        let mut writes = Vec::new();
        let mut reasons = Vec::new();
        let mut merged = false;
        let mut missing_keys = 0;
        for &name in category.files() {
            let Some((src, src_time)) = read(&source.join(name))? else {
                reasons.push(format!("來源角色沒有 {name}"));
                continue;
            };
            let dst = read(&target.join(name))?;
            if !category.is_text()
                && let Err(why) = format::binary_prefix(&src)
            {
                reasons.push(format!("來源的 {name}：{why}"));
                continue;
            }
            let write = match dst {
                None if same_region => FileWrite { name: name.into(), bytes: src, modified: src_time },
                None => {
                    reasons.push(format!("目標角色沒有 {name}，跨區服無法確認格式"));
                    continue;
                }
                Some((dst, dst_time)) if category.is_text() => {
                    let result = format::merge_text(&dst, &src);
                    merged = true;
                    missing_keys += result.missing_keys;
                    FileWrite { name: name.into(), bytes: result.bytes, modified: dst_time }
                }
                Some((dst, dst_time)) => match (format::binary_prefix(&dst), format::binary_prefix(&src)) {
                    (Err(why), _) => {
                        reasons.push(format!("目標的 {name}：{why}"));
                        continue;
                    }
                    (Ok(d), Ok(s)) if d == s => FileWrite { name: name.into(), bytes: src, modified: dst_time },
                    _ => {
                        reasons.push(format!("{name} 兩邊格式不同（類型或大小）"));
                        continue;
                    }
                },
            };
            writes.push(write);
        }
        let action = if !reasons.is_empty() {
            Action::Skip
        } else if merged {
            Action::Merge
        } else {
            Action::Copy
        };
        let reason = (!reasons.is_empty()).then(|| reasons.join("；"));
        if action == Action::Skip {
            writes.clear();
        }
        out.push((CategoryPlan { category, action, reason, missing_keys }, writes));
    }
    Ok(out)
}

/// 轉移計畫（不寫入）。
pub fn plan(source: &Path, target: &Path, same_region: bool) -> AppResult<Vec<CategoryPlan>> {
    Ok(plan_writes(source, target, same_region)?.into_iter().map(|(plan, _)| plan).collect())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferResult {
    pub done: Vec<Category>,
    /// 選了但不可轉的分類（含原因）。
    pub skipped: Vec<CategoryPlan>,
    /// 寫入前的自動備份；沒有任何可轉的分類時沒有寫入，也不備份。
    pub backup: Option<String>,
}

/// 重算計畫，只做 `selected` 中可轉的分類：自動備份目標 → 寫入。呼叫端須已通過 [`ensure_writable`]（目標區服）。
pub fn transfer(
    source: &Path,
    game_dir: &Path,
    backups_dir: &Path,
    id: &str,
    same_region: bool,
    selected: &[Category],
) -> AppResult<TransferResult> {
    let target = character_dir(game_dir, id);
    if !target.is_dir() {
        return Err(AppError::NotFound {
            what: format!("角色資料夾 {}", target.display()),
        });
    }
    let mut done = Vec::new();
    let mut skipped = Vec::new();
    let mut writes = Vec::new();
    for (plan, files) in plan_writes(source, &target, same_region)? {
        if !selected.contains(&plan.category) {
            continue;
        }
        if plan.action == Action::Skip {
            skipped.push(plan);
        } else {
            done.push(plan.category);
            writes.extend(files);
        }
    }
    let backup = if writes.is_empty() { None } else { Some(apply(game_dir, backups_dir, id, &writes)?) };
    Ok(TransferResult { done, skipped, backup })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0040000002A24D5B";

    struct Fixture {
        _dir: tempfile::TempDir,
        game: PathBuf,
        backups: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let game = dir.path().join("game");
            let backups = dir.path().join("backups");
            fs::create_dir_all(game.join(folder_name(ID))).expect("mkdir");
            Self { _dir: dir, game, backups }
        }

        fn char_dir(&self, id: &str) -> PathBuf {
            character_dir(&self.game, id)
        }

        fn put(&self, id: &str, name: &str, bytes: &[u8], secs: u64) {
            let dir = self.char_dir(id);
            fs::create_dir_all(&dir).expect("mkdir");
            store::write_new_at(&dir.join(name), bytes, Some(at(secs))).or_else(|_| {
                fs::remove_file(dir.join(name)).expect("remove");
                store::write_new_at(&dir.join(name), bytes, Some(at(secs)))
            })
            .expect("write");
        }

        /// 角色資料夾頂層：檔名 → (內容, 修改時間)。
        fn top(&self, id: &str) -> std::collections::BTreeMap<String, (Vec<u8>, SystemTime)> {
            let dir = self.char_dir(id);
            top_files(&dir)
                .expect("list")
                .into_iter()
                .map(|n| (n.to_string_lossy().into_owned(), read_with_time(&dir.join(&n)).expect("read")))
                .collect()
        }
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + std::time::Duration::from_secs(secs)
    }

    fn binary(kind: u32, max_len: u32, fill: u8) -> Vec<u8> {
        let mut bytes = vec![fill; max_len as usize + 32];
        bytes[..4].copy_from_slice(&kind.to_le_bytes());
        bytes[4..8].copy_from_slice(&max_len.to_le_bytes());
        bytes[8..12].copy_from_slice(&0u32.to_le_bytes());
        bytes
    }

    #[test]
    fn ids_must_be_16_hex_digits() {
        assert!(is_valid_id(ID));
        assert!(is_valid_id("004000028fabd965"));
        for bad in ["", "0040000002A24D5", "0040000002A24D5BX", "..\\..\\..\\abcdef", "0040000002A24D5G"] {
            assert!(!is_valid_id(bad), "{bad}");
        }
        assert_eq!(parse_backup_id("20261021-123456-789-manual"), Some(BackupKind::Manual));
        assert_eq!(parse_backup_id("20261021-123456-789-auto"), Some(BackupKind::Auto));
        for bad in ["20261021-123456-789-other", "..\\20261021-123456-789-auto", "2026102-123456-789-auto", ".x.xlm-tmp"] {
            assert_eq!(parse_backup_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn list_reads_only_character_folders_and_main_file_times() {
        let f = Fixture::new();
        f.put(ID, "HOTBAR.DAT", b"x", 100);
        f.put(ID, "COMMON.DAT", b"x", 300);
        // .old／.bak 不算最後遊玩時間。
        f.put(ID, "COMMON.DAT.old", b"x", 900);
        f.put(ID, "UISAVE.DAT.bak-20260803-012433", b"x", 900);
        let other = "004000028FABD965";
        f.put(other, "MACRO.DAT", b"x", 500);
        fs::create_dir_all(f.game.join("FFXIV_CHR1234")).expect("mkdir");
        fs::create_dir_all(f.game.join("screenshots")).expect("mkdir");
        let found = list(&f.game, &f.backups).expect("list");
        let ids: Vec<_> = found.iter().map(|c| (c.id.as_str(), c.last_played)).collect();
        assert_eq!(ids, vec![(other, Some(500)), (ID, Some(300))]);
        assert!(list(&f.game.join("missing"), &f.backups).expect("missing").is_empty());
    }

    #[test]
    fn backup_copies_top_files_and_shared_but_not_subfolders() {
        let f = Fixture::new();
        f.put(ID, "HOTBAR.DAT", b"hotbar", 100);
        fs::create_dir_all(f.char_dir(ID).join("log")).expect("mkdir");
        fs::write(f.char_dir(ID).join("log").join("00000000.log"), b"chat").expect("write");
        fs::write(f.game.join("FFXIV.cfg"), b"cfg").expect("write");
        let id = backup(&f.game, &f.backups, ID, BackupKind::Manual).expect("backup");
        let dir = backups_of(&f.backups, ID).join(&id);
        let (bytes, modified) = read_with_time(&dir.join("files").join("HOTBAR.DAT")).expect("read");
        assert_eq!((bytes.as_slice(), modified), (b"hotbar".as_slice(), at(100)));
        assert!(!dir.join("files").join("log").exists());
        assert_eq!(fs::read(dir.join("shared").join("FFXIV.cfg")).expect("cfg"), b"cfg");
        assert!(!dir.join("shared").join("MACROSYS.dat").exists());
        assert_eq!(backups(&f.backups, ID).expect("list"), vec![BackupInfo { id, kind: BackupKind::Manual }]);
    }

    #[test]
    fn restore_writes_backup_files_keeps_extra_files_and_backs_up_first() {
        let f = Fixture::new();
        f.put(ID, "HOTBAR.DAT", b"old", 100);
        f.put(ID, "MACRO.DAT", b"old macro", 110);
        let saved = backup(&f.game, &f.backups, ID, BackupKind::Manual).expect("backup");
        f.put(ID, "HOTBAR.DAT", b"new", 200);
        fs::remove_file(f.char_dir(ID).join("MACRO.DAT")).expect("remove");
        // 備份之後才出現、使用者自己另存的檔：還原不動。
        f.put(ID, "UISAVE.DAT.bak-20260803-012433", b"user", 150);
        let before = f.top(ID);

        let auto = restore(&f.game, &f.backups, ID, &saved).expect("restore");
        let after = f.top(ID);
        assert_eq!(after["HOTBAR.DAT"], (b"old".to_vec(), at(100)));
        assert_eq!(after["MACRO.DAT"], (b"old macro".to_vec(), at(110)));
        assert_eq!(after["UISAVE.DAT.bak-20260803-012433"], (b"user".to_vec(), at(150)));
        // 還原前的狀態在自動備份裡。
        let auto_files = backups_of(&f.backups, ID).join(&auto).join("files");
        for (name, content) in before {
            assert_eq!(read_with_time(&auto_files.join(&name)).expect("auto"), content, "{name}");
        }
    }

    #[test]
    fn failed_write_restores_written_files_and_removes_created_ones() {
        let f = Fixture::new();
        f.put(ID, "HOTBAR.DAT", b"old", 100);
        let before = f.top(ID);
        // 已有滿額的自動備份與一份手動備份：失敗時一份都不能刪。
        let dir = backups_of(&f.backups, ID);
        for i in 0..AUTO_KEEP {
            let id = backup(&f.game, &f.backups, ID, BackupKind::Auto).expect("backup");
            fs::rename(dir.join(&id), dir.join(format!("2000{i:04}-000000-000-auto"))).expect("rename");
        }
        backup(&f.game, &f.backups, ID, BackupKind::Manual).expect("manual");
        // 第三個檔的目標是資料夾，取代必定失敗。
        fs::create_dir_all(f.char_dir(ID).join("BLOCKED.DAT")).expect("mkdir");
        let writes = [
            FileWrite { name: "HOTBAR.DAT".into(), bytes: b"new".to_vec(), modified: at(200) },
            FileWrite { name: "NEW.DAT".into(), bytes: b"created".to_vec(), modified: at(200) },
            FileWrite { name: "BLOCKED.DAT".into(), bytes: b"x".to_vec(), modified: at(200) },
        ];
        let err = apply(&f.game, &f.backups, ID, &writes).expect_err("must fail");
        assert!(matches!(err, AppError::CharacterWriteRolledBack { .. }), "{err}");
        assert_eq!(f.top(ID), before);
        // 失敗時不裁剪：原有的 10 份自動、1 份手動，加上這次的自動備份。
        let list = backups(&f.backups, ID).expect("list");
        assert_eq!(list.iter().filter(|b| b.kind == BackupKind::Auto).count(), AUTO_KEEP + 1);
        assert_eq!(list.len(), AUTO_KEEP + 2);
    }

    /// 恢復也失敗時：自動備份改成手動備份；只有這次新建、還留著的檔列為要刪除（改名後不能再拿備份判斷）。
    #[test]
    fn failed_rollback_keeps_backup_as_manual_and_lists_only_created_files() {
        let f = Fixture::new();
        f.put(ID, "HOTBAR.DAT", b"old", 100);
        f.put(ID, "BLOCKED.DAT", b"old", 100);
        let auto = backup(&f.game, &f.backups, ID, BackupKind::Auto).expect("backup");
        // 已寫入的狀態：HOTBAR 被覆寫、NEW 新建；BLOCKED 變成資料夾，恢復它必定失敗。
        f.put(ID, "HOTBAR.DAT", b"new", 200);
        f.put(ID, "NEW.DAT", b"created", 200);
        fs::remove_file(f.char_dir(ID).join("BLOCKED.DAT")).expect("remove");
        fs::create_dir_all(f.char_dir(ID).join("BLOCKED.DAT")).expect("mkdir");
        let written = ["HOTBAR.DAT", "NEW.DAT", "BLOCKED.DAT"].map(|name| FileWrite {
            name: name.into(),
            bytes: Vec::new(),
            modified: at(200),
        });
        let err = rollback(&f.char_dir(ID), &backups_of(&f.backups, ID), &auto, &written, &io::Error::other("disk full"));
        let AppError::CharacterRollbackFailed { backup: kept_as, created, .. } = err else { panic!("{err}") };
        assert_eq!(backups(&f.backups, ID).expect("list"), vec![BackupInfo { id: kept_as, kind: BackupKind::Manual }]);
        assert_eq!(created, "");
        assert_eq!(f.top(ID)["HOTBAR.DAT"], (b"old".to_vec(), at(100)));
        assert!(!f.char_dir(ID).join("NEW.DAT").exists());
    }

    #[test]
    fn auto_backups_are_pruned_only_after_success_and_manual_ones_are_kept() {
        let f = Fixture::new();
        f.put(ID, "HOTBAR.DAT", b"v", 100);
        let dir = backups_of(&f.backups, ID);
        let oldest = backup(&f.game, &f.backups, ID, BackupKind::Auto).expect("backup");
        let manual = backup(&f.game, &f.backups, ID, BackupKind::Manual).expect("manual");
        for i in 1..AUTO_KEEP {
            let id = backup(&f.game, &f.backups, ID, BackupKind::Auto).expect("backup");
            // 以名稱排序代表時間；同一毫秒建立時改名避免撞名。
            fs::rename(dir.join(&id), dir.join(format!("2099{i:04}-000000-000-auto"))).expect("rename");
        }
        // 已有 10 份自動備份時還原最舊那一份：先多一份自動備份，成功後才裁掉最舊的（就是剛還原的來源）。
        restore(&f.game, &f.backups, ID, &oldest).expect("restore oldest");
        let list = backups(&f.backups, ID).expect("list");
        assert_eq!(list.iter().filter(|b| b.kind == BackupKind::Auto).count(), AUTO_KEEP);
        assert!(list.iter().any(|b| b.id == manual));
        assert!(!list.iter().any(|b| b.id == oldest));
    }

    /// 時鐘被調回：既有 10 份自動備份的 id 都比現在晚，這次寫入前的自動備份仍不能被裁掉。
    #[test]
    fn auto_backup_of_this_write_survives_a_clock_set_back() {
        let f = Fixture::new();
        f.put(ID, "HOTBAR.DAT", b"v", 100);
        let dir = backups_of(&f.backups, ID);
        let saved = backup(&f.game, &f.backups, ID, BackupKind::Manual).expect("manual");
        for i in 0..AUTO_KEEP {
            let id = backup(&f.game, &f.backups, ID, BackupKind::Auto).expect("backup");
            fs::rename(dir.join(&id), dir.join(format!("2099{i:04}-000000-000-auto"))).expect("rename");
        }
        let auto = restore(&f.game, &f.backups, ID, &saved).expect("restore");
        let list = backups(&f.backups, ID).expect("list");
        assert!(list.iter().any(|b| b.id == auto));
        assert_eq!(list.iter().filter(|b| b.kind == BackupKind::Auto).count(), AUTO_KEEP);
    }

    /// 角色資料夾是 junction：不備份、不寫入，連結另一端的檔不變。
    #[test]
    fn writes_into_a_linked_character_folder_are_refused() {
        let f = Fixture::new();
        f.put(ID, "HOTBAR.DAT", b"old", 100);
        let saved = backup(&f.game, &f.backups, ID, BackupKind::Manual).expect("manual");
        let real = f.game.join("elsewhere");
        fs::rename(f.char_dir(ID), &real).expect("move");
        crate::patch::zipatch::test_support::junction(&f.char_dir(ID), &real);
        fs::write(real.join("HOTBAR.DAT"), b"current").expect("write");

        let err = restore(&f.game, &f.backups, ID, &saved).expect_err("must refuse");
        assert!(matches!(err, AppError::CharacterLinked { .. }), "{err}");
        assert_eq!(fs::read(real.join("HOTBAR.DAT")).expect("read"), b"current");
        assert_eq!(backups(&f.backups, ID).expect("list").len(), 1);
    }

    #[test]
    fn transfer_plan_follows_header_and_region_rules() {
        let f = Fixture::new();
        let target = "004000028FABD965";
        let src = f.char_dir(ID);
        f.put(ID, "HOTBAR.DAT", &binary(7, 0x40, 1), 100);
        f.put(target, "HOTBAR.DAT", &binary(7, 0x40, 2), 200);
        // 最大長度不同（跨服 ADDON 的情形）。
        f.put(ID, "ADDON.DAT", &binary(9, 0x40, 1), 100);
        f.put(target, "ADDON.DAT", &binary(9, 0x50, 2), 200);
        // 目標的檔長與檔頭不符。
        f.put(ID, "KEYBIND.DAT", &binary(3, 0x40, 1), 100);
        let mut broken = binary(3, 0x40, 2);
        broken.extend_from_slice(b"extra");
        f.put(target, "KEYBIND.DAT", &broken, 200);
        // 目標缺檔。
        f.put(ID, "MACRO.DAT", &binary(5, 0x40, 1), 100);
        // 多檔分類只有一個檔可轉：整個分類不轉。
        f.put(ID, "CONTROL0.DAT", b"<A>\r\nk\t1\r\n", 100);
        f.put(target, "CONTROL0.DAT", b"<A>\r\nk\t0\r\n", 200);
        f.put(target, "CONTROL1.DAT", b"<A>\r\nk\t0\r\n", 200);
        f.put(ID, "COMMON.DAT", b"<A>\r\nk\t1\r\nnew\t1\r\n", 100);
        f.put(target, "COMMON.DAT", b"<A>\r\nk\t0\r\n", 200);

        let action = |plans: &[CategoryPlan], c: Category| {
            let p = plans.iter().find(|p| p.category == c).expect("category");
            (p.action, p.missing_keys)
        };
        let cross = plan(&src, &f.char_dir(target), false).expect("plan");
        assert_eq!(action(&cross, Category::Hotbar), (Action::Copy, 0));
        assert_eq!(action(&cross, Category::Hud), (Action::Skip, 0));
        assert_eq!(action(&cross, Category::Keybind), (Action::Skip, 0));
        assert_eq!(action(&cross, Category::Macro), (Action::Skip, 0));
        assert_eq!(action(&cross, Category::Controls), (Action::Skip, 0));
        assert_eq!(action(&cross, Category::CharacterSettings), (Action::Merge, 1));
        assert_eq!(action(&cross, Category::ChatFilter), (Action::Skip, 0));
        // 同區服：目標缺檔時直接複製。
        let same = plan(&src, &f.char_dir(target), true).expect("plan");
        assert_eq!(action(&same, Category::Macro), (Action::Copy, 0));
        // 同區服目標缺檔時，來源仍須完整。
        f.put(ID, "LOGFLTR.DAT", b"short", 100);
        let same = plan(&src, &f.char_dir(target), true).expect("plan");
        assert_eq!(action(&same, Category::ChatFilter), (Action::Skip, 0));
    }

    #[test]
    fn transfer_writes_selected_categories_with_target_times() {
        let f = Fixture::new();
        let target = "004000028FABD965";
        f.put(ID, "HOTBAR.DAT", &binary(7, 0x40, 1), 100);
        f.put(target, "HOTBAR.DAT", &binary(7, 0x40, 2), 200);
        f.put(ID, "COMMON.DAT", b"<A>\r\nk\t1\r\n", 100);
        f.put(target, "COMMON.DAT", b"<A>\r\nk\t0\r\n\x00", 200);
        f.put(ID, "MACRO.DAT", &binary(5, 0x40, 1), 100);
        f.put(target, "MACRO.DAT", &binary(5, 0x40, 2), 200);
        let selected = [Category::Hotbar, Category::CharacterSettings, Category::Hud];
        let result =
            transfer(&f.char_dir(ID), &f.game, &f.backups, target, true, &selected).expect("transfer");
        assert_eq!(result.done, vec![Category::Hotbar, Category::CharacterSettings]);
        assert_eq!(result.skipped.iter().map(|p| p.category).collect::<Vec<_>>(), vec![Category::Hud]);
        let top = f.top(target);
        assert_eq!(top["HOTBAR.DAT"], (binary(7, 0x40, 1), at(200)));
        assert_eq!(top["COMMON.DAT"], (b"<A>\r\nk\t1\r\n\x00".to_vec(), at(200)));
        // 沒選的分類不動。
        assert_eq!(top["MACRO.DAT"], (binary(5, 0x40, 2), at(200)));
        assert!(result.backup.is_some());
    }

    #[test]
    fn writes_are_refused_while_a_same_region_game_runs_from_any_install() {
        use crate::patch::running::test_support::{FakeProcess, GAME_PROCESSES};
        let _processes = GAME_PROCESSES.blocking_write();
        let dir = tempfile::tempdir().expect("tempdir");
        let install = |name: &str| {
            let root = dir.path().join(name);
            fs::create_dir_all(root.join("boot")).expect("mkdir");
            root
        };
        let (tw, global, tw_copy) = (install("tw"), install("global"), install("tw-copy"));
        let mut config = Config::default();
        assert!(matches!(ensure_writable(&config, Region::Tw), Err(AppError::GameNotConfigured { .. })));
        config.games.set(Region::Tw, Some(tw));
        config.games.set(Region::Global, Some(global.clone()));

        // 另一區服設定的遊戲執行檔不算同區服客戶端。
        let global_game = FakeProcess::start(&crate::game::game_exe(&global));
        let excluded = other_region_exes(&config, Region::Tw);
        assert!(running::game_clients(&[]).expect("scan").contains(&global_game.pid()));
        // 程序剛啟動的瞬間查不到映像路徑，依保守規則會算進去；等它查得到再判定（最多約 5 秒）。
        let excluded_eventually = (0..100).any(|_| {
            let found = running::game_clients(&excluded).expect("scan").contains(&global_game.pid());
            if found {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            !found
        });
        assert!(excluded_eventually);

        // 以另一個安裝路徑開的同區服遊戲：拒絕寫入。
        let _copy_game = FakeProcess::start(&crate::game::game_exe(&tw_copy));
        assert!(matches!(ensure_writable(&config, Region::Tw), Err(AppError::CharacterDataInUse { .. })));
    }
}
