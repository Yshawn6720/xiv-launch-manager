//! 國際服外部包與影子資料夾（protocols §1.6）：安裝資料夾維持官方原檔，外部包（字體、漢化）只放進
//! 影子資料夾，遊戲從影子的執行檔啟動。只用於國際服，台服不經過這裡。
//!
//! 影子在安裝根目錄的上一層，檔案都是 hardlink、資料夾是 junction。安裝在 Program Files 時一般權限建不了
//! hardlink，所以只在影子需要重建時（[`is_fresh`] 不成立）才經提權 helper（`patch::helper` 的
//! `--build-shadow`）重建，平常以一般權限沿用。重建時以 `fs::remove_dir_all` 整個刪掉：std 在 Windows 上
//! 只刪 junction 本身、不進入它指向的資料夾，刪 hardlink 只少一個名稱，安裝資料夾與外部包不受影響
//! （測試 `rebuild_keeps_install_and_pack`）。

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, GetFileInformationByHandle,
};
use windows::core::HSTRING;

use crate::error::{AppError, AppResult};
use crate::game;
use crate::patch::access::{self, AccessToken};
use crate::patch::helper::{self, HelperExit};
use crate::patch::job::is_plain_absolute;
use crate::patch::running;
use crate::region::Region;
use crate::store::ExternalPack;

const GAME_EXE: &str = "ffxiv_dx11.exe";
/// 影子資料夾名稱，位在安裝根目錄的上一層（protocols §1.6「影子資料夾」）。
const SHADOW_DIR: &str = "xiv-launch-manager-shadow";
/// 影子屬於哪個安裝：內容是安裝根目錄的完整路徑（UTF-8），建好後最後寫入。
const SOURCE_FILE: &str = "source";
/// dat 檔頭記錄資料段 SHA1 的位置（protocols §1.6「版本指紋」）。
const FINGERPRINT_OFFSET: u64 = 0x420;
/// `CreateHardLinkW`：來源與目的不在同一個磁碟區。
const ERROR_NOT_SAME_DEVICE: i32 = 17;
/// 不是一般檔案的屬性：資料夾或連結（reparse point）。
const NOT_PLAIN: u32 = FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0;

/// `install` 的影子根目錄：安裝根目錄上一層的 [`SHADOW_DIR`]。安裝根目錄沒有上一層（磁碟根目錄），或這個位置和
/// 安裝資料夾重疊（安裝資料夾就叫這個名稱，或經連結指到它；重建會整個刪掉影子資料夾）回 `None`。
pub fn root_for(install: &Path) -> Option<PathBuf> {
    let root = install.parent().filter(|p| !p.as_os_str().is_empty())?.join(SHADOW_DIR);
    (!overlaps(&resolved(&root), &resolved(install))).then_some(root)
}

/// 影子的遊戲執行檔（不確認存在）。
pub fn game_exe(root: &Path) -> PathBuf {
    root.join("game").join(GAME_EXE)
}

/// 外部包裡的一個 sqpack 檔。
#[derive(Debug, Clone, PartialEq, Eq)]
struct PackFile {
    /// 檔名，小寫（與安裝資料夾比對時不分大小寫）。
    name: String,
    path: PathBuf,
}

/// 讀過、格式合格的外部包：至少一組 sqpack，每組都有 `.dat0` 與 `.index`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pack {
    /// 外部包資料夾（讀取時的路徑）。
    dir: PathBuf,
    files: Vec<PackFile>,
    /// 各組的 id（6 位 hex，小寫），依名稱排序。
    ids: Vec<String>,
}

impl Pack {
    fn has(&self, lower_name: &str) -> bool {
        self.files.iter().any(|f| f.name == lower_name)
    }
}

/// sqpack 檔名 `{6 位 hex}.win32.{dat<N>|index|index2}` 的 id（小寫）；不符回 `None`。
fn sqpack_id(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    let (id, kind) = lower.split_once(".win32.")?;
    let valid_id = id.len() == 6 && id.bytes().all(|b| b.is_ascii_hexdigit());
    let valid_kind = matches!(kind, "index" | "index2")
        || kind.strip_prefix("dat").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    (valid_id && valid_kind).then(|| id.to_string())
}

fn invalid(reason: impl Into<String>) -> AppError {
    AppError::ExternalPackInvalid { reason: reason.into() }
}

/// 讀外部包資料夾：只收頂層、檔名符合 sqpack 規則的一般檔案（不跟隨連結：影子以 hardlink 連到這些檔），
/// 其他檔案、連結與子資料夾不理會。
pub fn read_pack(dir: &Path) -> AppResult<Pack> {
    let entries = fs::read_dir(dir).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => invalid(format!("{} 不存在", dir.display())),
        _ => AppError::io(format!("讀取 {}", dir.display()), e),
    })?;
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| AppError::io(format!("讀取 {}", dir.display()), e))?;
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if sqpack_id(&name).is_none() {
            continue;
        }
        if fs::symlink_metadata(entry.path()).is_ok_and(|m| m.is_file()) {
            files.push(PackFile { name, path: entry.path() });
        }
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    let mut ids: Vec<String> = files.iter().filter_map(|f| sqpack_id(&f.name)).collect();
    ids.dedup();
    if ids.is_empty() {
        return Err(invalid("找不到 sqpack 檔（例如 000000.win32.dat0、0a0000.win32.index）"));
    }
    let has = |name: String| files.iter().any(|f| f.name == name);
    for id in &ids {
        for ext in ["dat0", "index"] {
            if !has(format!("{id}.win32.{ext}")) {
                return Err(invalid(format!("缺少 {id}.win32.{ext}")));
            }
        }
    }
    Ok(Pack { dir: dir.to_path_buf(), files, ids })
}

/// dat 檔頭偏移 0x420 的 20 bytes（資料段 SHA1，protocols §1.6）。
fn fingerprint(dat0: &Path) -> io::Result<[u8; 20]> {
    let mut file = File::open(dat0)?;
    file.seek(SeekFrom::Start(FINGERPRINT_OFFSET))?;
    let mut out = [0u8; 20];
    file.read_exact(&mut out)?;
    Ok(out)
}

fn sqpack_dir(install_or_shadow: &Path) -> PathBuf {
    install_or_shadow.join("game").join("sqpack").join("ffxiv")
}

/// 開啟 `path`（檔案或資料夾，只要求讀屬性權限）取屬性、磁碟區序號與 file index。`follow` 為假時不跟隨
/// 最後一層連結，取到的是連結本身。
fn file_info(path: &Path, follow: bool) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let no_follow = if follow { 0 } else { FILE_FLAG_OPEN_REPARSE_POINT.0 };
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | no_follow)
        .open(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: handle 由 file 持有、呼叫期間有效；info 是本函式存活中的區域變數。
    // quality-allow: Win32 FFI，GetFileInformationByHandle 沒有安全包裝
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }.map_err(io::Error::from)?;
    Ok(info)
}

/// 同一個檔（含它的每個 hardlink）的識別：磁碟區序號與 file index。
fn file_id(info: &BY_HANDLE_FILE_INFORMATION) -> (u32, u64) {
    (info.dwVolumeSerialNumber, u64::from(info.nFileIndexHigh) << 32 | u64::from(info.nFileIndexLow))
}

/// 比對用的路徑：存在的最長上層以 `canonicalize` 解析（展開 junction、8.3 短名），其餘照字面接上，再轉成
/// `running::comparable` 形式。影子資料夾可能還沒建，不能要求整條路徑存在。
fn resolved(path: &Path) -> String {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(mut full) = fs::canonicalize(existing) {
            full.extend(rest.iter().rev());
            return running::comparable(&full);
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                existing = parent;
            }
            _ => return running::comparable(path),
        }
    }
}

/// 兩個 [`resolved`] 路徑相同，或一個在另一個裡面。
fn overlaps(a: &str, b: &str) -> bool {
    let inside = |x: &str, y: &str| x.strip_prefix(y).is_some_and(|rest| rest.is_empty() || rest.starts_with('\\'));
    inside(a, b) || inside(b, a)
}

/// 外部包資料夾的位置（protocols §1.6）：一般的完整路徑（同 helper 收的參數），且不與影子資料夾、安裝資料夾
/// 重疊（重建影子會整個刪掉影子資料夾；安裝資料夾由 patch 寫入）。
fn check_location(dir: &Path, install: Option<&Path>) -> AppResult<()> {
    if !is_plain_absolute(dir) {
        return Err(invalid(format!("{} 不是一般的完整路徑（例如 D:\\xlm-pack，不含 . 或 ..）", dir.display())));
    }
    let pack = resolved(dir);
    let root = install.and_then(root_for);
    for (other, what) in [(root.as_deref(), "影子資料夾"), (install, "國際服安裝資料夾")] {
        if let Some(other) = other
            && overlaps(&pack, &resolved(other))
        {
            return Err(invalid(format!("不能放在{what}（{}）裡面，也不能包含它", other.display())));
        }
    }
    Ok(())
}

/// 外部包、安裝資料夾與影子的上層資料夾在同一個磁碟區（跟隨連結，看實際位置）：影子在安裝上一層，以
/// hardlink 連到安裝與外部包的檔，hardlink 不能跨磁碟區。安裝資料夾本身是指向別的磁碟的 junction 時也不行。
fn check_volume(dir: &Path, install: &Path) -> AppResult<()> {
    let volume = |path: &Path| {
        file_info(path, true)
            .map(|info| info.dwVolumeSerialNumber)
            .map_err(|e| AppError::io(format!("讀取 {} 所在的磁碟", path.display()), e))
    };
    let expected = volume(dir)?;
    for other in [Some(install), install.parent()].into_iter().flatten() {
        if volume(other)? != expected {
            return Err(invalid(
                "必須和國際服安裝資料夾（含它的上一層）在同一個磁碟（影子資料夾以 hardlink 連到外部包，hardlink 不能跨磁碟）",
            ));
        }
    }
    Ok(())
}

/// 開啟外部包與每次啟動前的檢查（protocols §1.6「外部包」）：位置合格、有可用的 sqpack 檔，且和安裝資料夾
/// 在同一個磁碟區。還沒設定國際服安裝資料夾時只檢查前兩項以外的部分（啟動時再全部檢查）。
pub fn check_dir(dir: &Path, install: Option<&Path>) -> AppResult<Pack> {
    check_location(dir, install)?;
    let pack = read_pack(dir)?;
    if let Some(install) = install {
        check_volume(dir, install)?;
    }
    Ok(pack)
}

/// 外部包每組 `.dat0` 的指紋都等於安裝資料夾同名 `.dat0` 才算目前版本的包；安裝資料夾沒有同名
/// `.dat0` 也算過期。讀不到檔頭（檔案太短、讀取失敗）回錯誤。
pub fn is_current(pack: &Pack, install: &Path) -> AppResult<bool> {
    let read = |path: &Path| fingerprint(path).map_err(|e| AppError::io(format!("讀取 {} 的檔頭", path.display()), e));
    for id in &pack.ids {
        let name = format!("{id}.win32.dat0");
        let original = sqpack_dir(install).join(&name);
        if !original.is_file() {
            return Ok(false);
        }
        let packed = pack.files.iter().find(|f| f.name == name).map(|f| f.path.as_path());
        let Some(packed) = packed else {
            return Ok(false);
        };
        if read(packed)? != read(&original)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 影子裡一項應有的樣子。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// 實體資料夾。
    Dir,
    /// junction，指向安裝資料夾裡的這個資料夾。
    Junction(PathBuf),
    /// hardlink，和這個檔是同一個檔。
    File(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    /// 相對影子根目錄的路徑。
    rel: PathBuf,
    kind: Kind,
}

fn read_dir_error(dir: &Path) -> impl Fn(io::Error) -> AppError + '_ {
    move |e| AppError::io(format!("讀取 {}", dir.display()), e)
}

/// 影子應有的內容（protocols §1.6「影子資料夾」內容），資料夾排在它的內容之前：`game`、`game\sqpack` 兩層
/// 的子資料夾 junction 到安裝資料夾（`sqpack`、`ffxiv` 除外），檔案 hardlink（含執行檔）；`game\sqpack\ffxiv`
/// 收安裝裡的檔案（外部包有的除外、子資料夾不收），再加上外部包的檔案。
pub(crate) fn plan(install: &Path, pack: &Pack) -> AppResult<Vec<Entry>> {
    let game = PathBuf::from("game");
    let sqpack = game.join("sqpack");
    let ffxiv = sqpack.join("ffxiv");
    let mut out = vec![Entry { rel: game.clone(), kind: Kind::Dir }];
    plan_children(install, &game, &mut out, "sqpack")?;
    out.push(Entry { rel: sqpack.clone(), kind: Kind::Dir });
    plan_children(install, &sqpack, &mut out, "ffxiv")?;
    out.push(Entry { rel: ffxiv.clone(), kind: Kind::Dir });
    let dir = install.join(&ffxiv);
    for entry in fs::read_dir(&dir).map_err(read_dir_error(&dir))? {
        let entry = entry.map_err(read_dir_error(&dir))?;
        let src = entry.path();
        let name = entry.file_name();
        if fs::metadata(&src).map_err(read_dir_error(&src))?.is_dir()
            || pack.has(&name.to_string_lossy().to_ascii_lowercase())
        {
            continue;
        }
        out.push(Entry { rel: ffxiv.join(&name), kind: Kind::File(src) });
    }
    for file in &pack.files {
        out.push(Entry { rel: ffxiv.join(&file.name), kind: Kind::File(file.path.clone()) });
    }
    Ok(out)
}

/// 安裝資料夾 `rel` 下每一項：資料夾 junction、檔案 hardlink；名稱（不分大小寫）為 `nested` 的資料夾另外處理。
fn plan_children(install: &Path, rel: &Path, out: &mut Vec<Entry>, nested: &str) -> AppResult<()> {
    let dir = install.join(rel);
    for entry in fs::read_dir(&dir).map_err(read_dir_error(&dir))? {
        let entry = entry.map_err(read_dir_error(&dir))?;
        let src = entry.path();
        let name = entry.file_name();
        let is_dir = fs::metadata(&src).map_err(read_dir_error(&src))?.is_dir();
        if is_dir && name.to_string_lossy().eq_ignore_ascii_case(nested) {
            continue;
        }
        let kind = if is_dir { Kind::Junction(src) } else { Kind::File(src) };
        out.push(Entry { rel: rel.join(&name), kind });
    }
    Ok(())
}

fn shadow_error(reason: impl Into<String>) -> AppError {
    AppError::ShadowBuild { reason: reason.into() }
}

fn link_error(e: io::Error) -> AppError {
    if e.raw_os_error() == Some(ERROR_NOT_SAME_DEVICE) {
        shadow_error("外部包和遊戲不在同一個磁碟")
    } else if e.kind() == io::ErrorKind::PermissionDenied {
        shadow_error("沒有權限建立連結（遊戲或外部包的檔案不允許目前的使用者建立連結）")
    } else {
        AppError::io("建立影子資料夾的連結", e)
    }
}

/// 在 `dst` 建 `src` 的 hardlink（protocols §1.6「重建 helper」第 7 步）：先以不跟隨連結的方式確認來源是
/// 一般檔案，建立後確認新連結和來源是同一個檔，不同（建立途中來源被換掉）就刪掉新連結並失敗。
fn link_file(src: &Path, dst: &Path) -> AppResult<()> {
    let source = file_info(src, false).map_err(read_dir_error(src))?;
    if source.dwFileAttributes & NOT_PLAIN != 0 {
        return Err(shadow_error(format!("{} 不是一般檔案", src.display())));
    }
    fs::hard_link(src, dst).map_err(link_error)?;
    let linked = file_info(dst, false).map_err(read_dir_error(dst))?;
    if file_id(&linked) != file_id(&source) {
        fs::remove_file(dst).map_err(|e| AppError::io("刪除連錯的影子檔案", e))?;
        return Err(shadow_error(format!("{} 在建立連結時被換掉", src.display())));
    }
    Ok(())
}

/// 影子根目錄的 DACL（SDDL）：受保護、不繼承上層；SYSTEM 與 Administrators 完全控制，Users 讀取與執行；
/// 子項（資料夾、junction）都繼承這三項。
const ROOT_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)";

/// 以 [`ROOT_SDDL`] 建立影子根目錄（提權 helper 用，protocols §1.6「重建 helper」第 3 步）：影子裡的資料夾從建立
/// 起就不受上層權限影響，一般程序無法在建立途中把它們換成連結。
pub fn create_protected_dir(path: &Path) -> io::Result<()> {
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: 輸入是以 0 結尾的常數字串；成功時 descriptor 指向系統以 LocalAlloc 配置的 security descriptor，
    // 由本函式在最後以 LocalFree 釋放。
    // quality-allow: Win32 FFI，ConvertStringSecurityDescriptorToSecurityDescriptorW 沒有安全包裝
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(&HSTRING::from(ROOT_SDDL), SDDL_REVISION_1, &mut descriptor, None)
    }
    .map_err(io::Error::from)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    // SAFETY: 路徑字串與 attributes 在呼叫期間存活；descriptor 在 LocalFree 之前有效，之後不再使用。
    // quality-allow: Win32 FFI，CreateDirectoryW 與 LocalFree 沒有安全包裝
    let created = unsafe { CreateDirectoryW(&HSTRING::from(path), Some(&attributes)) }.map_err(io::Error::from);
    // SAFETY: 同上。
    // quality-allow: Win32 FFI
    unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
    created
}

/// 刪掉整個影子再依 `plan`（[`plan`] 的結果）重建，最後寫 `source`；呼叫端須先確認遊戲沒有在執行。影子根目錄
/// 以 `create_root` 建立（提權 helper 傳 [`create_protected_dir`]，程式內重建傳 `fs::create_dir`），裡面的資料夾
/// 照常建立、繼承根目錄的權限。`plan` 由呼叫端先算：讀不到安裝資料夾時還沒動到影子（helper 回 10，不是 20）。
pub(crate) fn build(
    root: &Path,
    install: &Path,
    plan: &[Entry],
    create_root: impl FnOnce(&Path) -> io::Result<()>,
) -> AppResult<()> {
    match fs::remove_dir_all(root) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(AppError::io("刪除舊的影子資料夾", e)),
    }
    let mkdir_error = |e| AppError::io("建立影子資料夾", e);
    create_root(root).map_err(mkdir_error)?;
    for entry in plan {
        let dst = root.join(&entry.rel);
        match &entry.kind {
            Kind::Dir => fs::create_dir(&dst).map_err(mkdir_error)?,
            Kind::Junction(src) => junction::create(src, &dst).map_err(link_error)?,
            Kind::File(src) => link_file(src, &dst)?,
        }
    }
    fs::write(root.join(SOURCE_FILE), install.to_string_lossy().as_bytes())
        .map_err(|e| AppError::io("寫入影子資料夾來源", e))
}

/// 影子的一項是否和 [`plan`] 的要求相同：資料夾是實體資料夾；junction 指向同一個資料夾；檔案和來源是同一個檔。
fn entry_matches(root: &Path, entry: &Entry) -> bool {
    let dst = root.join(&entry.rel);
    let Ok(info) = file_info(&dst, false) else {
        return false;
    };
    let attributes = info.dwFileAttributes & NOT_PLAIN;
    match &entry.kind {
        Kind::Dir => attributes == FILE_ATTRIBUTE_DIRECTORY.0,
        Kind::Junction(src) => {
            junction::exists(&dst).unwrap_or(false)
                && matches!(
                    (running::normalize_path(&dst), running::normalize_path(src)),
                    (Ok(a), Ok(b)) if a == b
                )
        }
        Kind::File(src) => attributes == 0 && file_info(src, false).is_ok_and(|s| file_id(&s) == file_id(&info)),
    }
}

/// 影子的 `dir`（相對路徑）裡沒有 [`plan`] 以外的項目。
fn no_extra(root: &Path, dir: &Path, plan: &[Entry]) -> bool {
    let expected: HashSet<String> = plan
        .iter()
        .filter(|e| e.rel.parent() == Some(dir))
        .filter_map(|e| e.rel.file_name())
        .map(|name| name.to_string_lossy().to_lowercase())
        .collect();
    let Ok(entries) = fs::read_dir(root.join(dir)) else {
        return false;
    };
    entries
        .into_iter()
        .all(|e| e.is_ok_and(|e| expected.contains(&e.file_name().to_string_lossy().to_lowercase())))
}

/// 影子是否不用重建（protocols §1.6「是否需要重建」）：`source` 是這個安裝；[`plan`] 的每一項都相同；
/// `game`、`game\sqpack`、`game\sqpack\ffxiv` 裡沒有多出的項目。只讀取，一般權限就能判定；讀不到算要重建。
pub fn is_fresh(root: &Path, install: &Path, pack: &Pack) -> bool {
    if !matches!(belongs_to(root, install), Ok(true)) {
        return false;
    }
    let Ok(plan) = plan(install, pack) else {
        return false;
    };
    let game = Path::new("game");
    let sqpack = game.join("sqpack");
    let ffxiv = sqpack.join("ffxiv");
    plan.iter().all(|entry| entry_matches(root, entry))
        && [game, sqpack.as_path(), ffxiv.as_path()].into_iter().all(|dir| no_extra(root, dir, &plan))
}

/// 重建影子（protocols §1.6「重建」）：本程式沒有以系統管理員身分執行、影子上層資料夾以目前的身分就能修改、
/// 且遊戲沒有在執行時，在程式內 [`build`]；否則以 UAC 啟動 `--build-shadow` helper（以系統管理員身分執行時
/// 不會跳 UAC，由 helper 以未提權的身分判定上層）。helper 只認 HKLM 的安裝，設定的安裝不同時不啟動它。
/// 阻塞到完成（含 UAC 對話框），呼叫端以 `spawn_blocking` 包。
pub fn rebuild(root: &Path, install: &Path, pack: &Pack) -> AppResult<()> {
    let parent = root.parent().ok_or_else(|| shadow_error("影子資料夾沒有上層資料夾"))?;
    if !access::process_elevated()? && access::may_modify_dir(&AccessToken::current()?, parent)? {
        if !matches!(running::install_in_use(install), Ok(false)) {
            return Err(shadow_error("國際服遊戲正在執行"));
        }
        return build(root, install, &plan(install, pack)?, |dir| fs::create_dir(dir));
    }
    if let Some(reason) = helper_blocker(root, install, game::detect_install(Region::Global)) {
        return Err(shadow_error(reason));
    }
    let reason = match helper::run_shadow_elevated(install, &pack.dir)? {
        HelperExit::Success => return Ok(()),
        HelperExit::UacDeclined => "沒有同意系統管理員權限".to_string(),
        HelperExit::NotStarted => "無法以系統管理員權限啟動建立程式".to_string(),
        HelperExit::Busy => "遊戲正在更新".to_string(),
        HelperExit::AbortedBeforeWrite => {
            "建立前的檢查沒有通過（安裝路徑、遊戲執行中、外部包，或讀不到遊戲資料夾）".to_string()
        }
        HelperExit::InstallModifiable => {
            "影子資料夾的上層一般權限就能修改，不以系統管理員權限建立（以系統管理員身分執行本程式時會這樣，請以一般身分執行）"
                .to_string()
        }
        HelperExit::FailedAfterWrite => "建立到一半失敗".to_string(),
        HelperExit::Other(code) => format!("建立程式異常結束（{code}）"),
    };
    Err(shadow_error(reason))
}

/// 啟動 helper 前，以一般權限就能判定的「helper 必定中止或會建到別處」的原因（protocols §1.6「重建」）；
/// 沒有就回 `None`。`detected` 是 HKLM 的國際服安裝，helper 以它算影子位置：安裝不同，或同一個安裝經連結有
/// 兩個名稱、算出的影子位置不同時，helper 會中止或把影子建到別處（`rebuild` 回傳的影子執行檔就不存在）。
/// helper 持有的影子上層是 HKLM 字面路徑的上一層，它的最終路徑（展開連結）和字面路徑不同時 helper 也必定中止。
fn helper_blocker(root: &Path, install: &Path, detected: Option<PathBuf>) -> Option<String> {
    let helper_root = detected
        .filter(|detected| same_install(detected, install))
        .and_then(|detected| root_for(&detected));
    let Some(helper_root) = helper_root.filter(|helper_root| resolved(helper_root) == resolved(root)) else {
        return Some(
            "設定的國際服遊戲資料夾和登錄檔記載的不同（或兩者的影子資料夾位置不同），無法以系統管理員權限建立".to_string(),
        );
    };
    // root_for 的結果一定有上一層；沒有時拿影子位置本身比對（不存在，解析失敗而不啟動 helper）。
    let parent = helper_root.parent().unwrap_or(&helper_root);
    match running::normalize_path(parent) {
        Ok(actual) if actual == running::comparable(parent) => None,
        Ok(actual) => Some(format!(
            "影子資料夾的上層（{}）實際位置是 {actual}：路徑經過連結（junction 或 symlink），或用了短檔名、subst 磁碟。\
             系統管理員權限的建立不接受這種位置，所以沒有跳 UAC；要使用外部包，請移除連結、把資料夾搬回原處，\
             或讓登錄檔記載的國際服安裝位置（InstallLocation）指向實際位置",
            parent.display()
        )),
        Err(e) => Some(format!("無法確認影子資料夾上層的實際位置（{e}），不以系統管理員權限建立")),
    }
}

/// 影子裡建好之後才出現的項目（protocols §1.6「重建」）：影子根目錄與 `game`、`game\sqpack`、`game\sqpack\ffxiv`
/// 裡建立時間晚於 `source` 的檔案或資料夾，連結不算（刪除只刪連結本身）。hardlink 的建立時間是來源檔的，一定
/// 不晚於 `source`；晚於的是工具或使用者放進去的（例如寫在遊戲執行檔旁的截圖），重建會把它們一起刪掉。沒有
/// `source`（沒建過或沒建完）時無從判定，回空。
fn added_after_build(root: &Path) -> AppResult<Vec<PathBuf>> {
    let source_error = |e| AppError::io("讀取影子資料夾來源", e);
    let built = match fs::symlink_metadata(root.join(SOURCE_FILE)) {
        Ok(meta) => meta.created().map_err(source_error)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(source_error(e)),
    };
    let game = root.join("game");
    let sqpack = game.join("sqpack");
    let ffxiv = sqpack.join("ffxiv");
    let mut added = Vec::new();
    for dir in [root.to_path_buf(), game, sqpack, ffxiv] {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(read_dir_error(&dir)(e)),
        };
        for entry in entries {
            let path = entry.map_err(read_dir_error(&dir))?.path();
            let meta = fs::symlink_metadata(&path).map_err(read_dir_error(&path))?;
            let is_link = meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;
            if !is_link && meta.created().map_err(read_dir_error(&path))? > built {
                added.push(path);
            }
        }
    }
    Ok(added)
}

/// 影子 `source` 記的安裝；沒有 `source`（沒建過或沒建完）回 `None`。
fn read_source(root: &Path) -> AppResult<Option<PathBuf>> {
    match fs::read_to_string(root.join(SOURCE_FILE)) {
        Ok(text) => Ok(Some(PathBuf::from(text))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(AppError::io("讀取影子資料夾來源", e)),
    }
}

fn same_install(source: &Path, install: &Path) -> bool {
    running::normalize_path(source).is_ok_and(|s| running::normalize_path(install).is_ok_and(|i| s == i))
}

/// 影子 `source` 記的安裝是否就是 `install`；沒有 `source` 回 `false`（啟動時不沿用來歷不明的影子）。
fn belongs_to(root: &Path, install: &Path) -> AppResult<bool> {
    Ok(read_source(root)?.is_some_and(|source| same_install(&source, install)))
}

/// 屬於 `install` 的影子執行檔（存在才回）；供「安裝正在使用」判定（protocols §3 例外）。`source` 不存在
/// 或讀不到但執行檔存在時保守算屬於（寧可誤擋更新，也不在影子的遊戲執行時更新）。
pub fn exe_of(root: &Path, install: &Path) -> Option<PathBuf> {
    let exe = game_exe(root);
    let belongs = match read_source(root) {
        Ok(Some(source)) => same_install(&source, install),
        Ok(None) | Err(_) => true,
    };
    (exe.is_file() && belongs).then_some(exe)
}

/// 存設定前的檢查（protocols §1.6「確認」）：開啟時須有資料夾，且已確認過風險（之前確認過，或這次
/// `accept_risk`）。確認只記一次，之後開關不再問。資料夾內容由呼叫端先以 [`check_dir`] 檢查。
pub fn apply_settings(pack: &mut ExternalPack, dir: Option<PathBuf>, enabled: bool, accept_risk: bool) -> AppResult<()> {
    if enabled && dir.is_none() {
        return Err(invalid("請先填外部包資料夾"));
    }
    let risk_accepted = pack.risk_accepted || accept_risk;
    if enabled && !risk_accepted {
        return Err(AppError::ExternalPackRiskNotAccepted);
    }
    *pack = ExternalPack { dir, enabled, risk_accepted };
    Ok(())
}

/// 這次國際服要從哪個執行檔啟動，以及要給使用者的提示（外部包沒套用的原因）。
#[derive(Debug, PartialEq, Eq)]
pub struct LaunchExe {
    pub exe: PathBuf,
    pub warning: Option<String>,
}

/// 依設定決定啟動的執行檔（protocols §1.6「啟動」）：關閉或未確認風險時用原執行檔；外部包讀不到、過期或
/// 影子建不起來時也用原執行檔並附原因；否則從影子啟動。影子的遊戲正在執行就沿用（無法確認時用原執行檔），
/// [`is_fresh`] 成立也沿用；其餘在影子裡沒有建好之後才放進去的項目時呼叫 `rebuild(影子根目錄, 外部包)`
/// （正式流程是 [`rebuild`]），有就不重建、用原執行檔並列出它們。
pub fn launch_exe(
    settings: &ExternalPack,
    install: &Path,
    rebuild: impl FnOnce(&Path, &Pack) -> AppResult<()>,
) -> LaunchExe {
    let original = crate::game::game_exe(install);
    let (true, true, Some(dir)) = (settings.enabled, settings.risk_accepted, settings.dir.as_deref()) else {
        return LaunchExe { exe: original, warning: None };
    };
    let fallback = |reason: String| LaunchExe {
        exe: original.clone(),
        warning: Some(format!("{reason}，這次以原版遊戲啟動")),
    };
    let Some(root) = root_for(install) else {
        return fallback(
            "外部包沒有套用（沒有地方放影子資料夾：遊戲安裝在磁碟根目錄，或安裝資料夾就在影子資料夾的位置）".into(),
        );
    };
    let pack = match check_dir(dir, Some(install)) {
        Ok(pack) => pack,
        Err(e) => return fallback(format!("外部包沒有套用（{e}）")),
    };
    match is_current(&pack, install) {
        Ok(true) => {}
        Ok(false) => return fallback("外部包已過期（與目前的遊戲版本不符，請用新版官方檔重新產生）".into()),
        Err(e) => return fallback(format!("外部包沒有套用（{e}）")),
    }
    let exe = game_exe(&root);
    let in_use = match exe.is_file().then(|| running::exe_running(&exe)) {
        None | Some(Ok(false)) => false,
        Some(Ok(true)) => true,
        Some(Err(e)) => return fallback(format!("外部包沒有套用（無法確認影子的遊戲是否在執行：{e}）")),
    };
    if in_use {
        return match belongs_to(&root, install) {
            Ok(true) => LaunchExe { exe, warning: None },
            Ok(false) => fallback("外部包沒有套用（影子資料夾正被另一個安裝的遊戲使用）".into()),
            Err(e) => fallback(format!("外部包沒有套用（{e}）")),
        };
    }
    if !is_fresh(&root, install, &pack) {
        match added_after_build(&root) {
            Ok(added) if added.is_empty() => {}
            Ok(added) => return fallback(added_warning(&added)),
            Err(e) => return fallback(format!("外部包沒有套用（{e}）")),
        }
        if let Err(e) = rebuild(&root, &pack) {
            return fallback(format!("外部包沒有套用（{e}）"));
        }
    }
    LaunchExe { exe, warning: None }
}

/// [`added_after_build`] 找到項目時的提示：列前 3 個路徑，其餘只給數量。
fn added_warning(added: &[PathBuf]) -> String {
    const SHOWN: usize = 3;
    let mut list = added.iter().take(SHOWN).map(|p| p.display().to_string()).collect::<Vec<_>>().join("、");
    if added.len() > SHOWN {
        list.push_str(&format!(" 等 {} 項", added.len()));
    }
    format!("外部包沒有套用（影子資料夾裡有建好之後才放進去的檔案，重建影子會刪掉它們，請先移到別處：{list}）")
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io::Write;

    use super::*;
    use crate::patch::running::test_support::{FakeProcess, GAME_PROCESSES};

    /// 官方 dat0：檔頭 0x420 起放 `stamp` 重複 20 次當指紋，後面接資料。
    fn dat0(stamp: u8, tail: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0u8; 0x800];
        bytes[0x420..0x434].fill(stamp);
        bytes.extend_from_slice(tail);
        bytes
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, bytes).expect("write");
    }

    /// 以新檔取代（patch 刪掉舊檔再寫新檔的情況）：原本的 hardlink 仍指向舊檔。
    fn replace(path: &Path, bytes: &[u8]) {
        fs::remove_file(path).expect("remove");
        fs::write(path, bytes).expect("write");
    }

    fn same_file(a: &Path, b: &Path) -> bool {
        file_id(&file_info(a, false).expect("a")) == file_id(&file_info(b, false).expect("b"))
    }

    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        /// 安裝：game 根目錄的執行檔、ver、movie 資料夾；sqpack\ex1；sqpack\ffxiv 的兩組官方檔與使用者的
        /// 備份子資料夾。外部包：000000 一組（指紋同官方）。
        fn new() -> Self {
            let f = Self { dir: tempfile::tempdir().expect("tempdir") };
            let game = f.install().join("game");
            write(&game.join(GAME_EXE), b"exe");
            write(&game.join("ffxivgame.ver"), b"2026.10.16.0000.0000");
            write(&game.join(r"movie\ffxiv\00000.bk2"), b"movie");
            write(&game.join(r"sqpack\ex1\020100.win32.dat0"), b"ex1");
            let ffxiv = sqpack_dir(&f.install());
            write(&ffxiv.join("000000.win32.dat0"), &dat0(1, b"official font"));
            write(&ffxiv.join("000000.win32.index"), b"official index");
            write(&ffxiv.join("0a0000.win32.dat0"), &dat0(2, b"official text"));
            write(&ffxiv.join(r"backup\000000.win32.dat0"), b"user backup");
            write(&f.pack().join("000000.win32.dat0"), &dat0(1, b"official font + appended font"));
            write(&f.pack().join("000000.win32.index"), b"mod index");
            write(&f.pack().join("000000.win32.index2"), b"index2");
            write(&f.pack().join("readme.txt"), b"ignored");
            f
        }

        fn install(&self) -> PathBuf {
            self.dir.path().join("FINAL FANTASY XIV - A Realm Reborn")
        }

        fn pack(&self) -> PathBuf {
            self.dir.path().join("pack")
        }

        fn root(&self) -> PathBuf {
            root_for(&self.install()).expect("root")
        }

        fn read_pack(&self) -> Pack {
            read_pack(&self.pack()).expect("pack")
        }

        fn build(&self) {
            let install = self.install();
            let plan = plan(&install, &self.read_pack()).expect("plan");
            build(&self.root(), &install, &plan, |dir| fs::create_dir(dir)).expect("build");
        }

        fn fresh(&self) -> bool {
            is_fresh(&self.root(), &self.install(), &self.read_pack())
        }

        fn settings(&self) -> ExternalPack {
            ExternalPack { dir: Some(self.pack()), enabled: true, risk_accepted: true }
        }
    }

    #[test]
    fn shadow_root_is_beside_install() {
        assert_eq!(
            root_for(Path::new(r"C:\Program Files (x86)\SquareEnix\FINAL FANTASY XIV - A Realm Reborn")),
            Some(PathBuf::from(r"C:\Program Files (x86)\SquareEnix\xiv-launch-manager-shadow"))
        );
        assert_eq!(root_for(Path::new(r"D:\")), None, "安裝在磁碟根目錄");
        assert_eq!(root_for(Path::new(r"D:\Games\XIV-Launch-Manager-Shadow")), None, "安裝資料夾就叫影子的名稱");

        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join(SHADOW_DIR);
        fs::create_dir(&real).expect("mkdir");
        let alias = dir.path().join("FINAL FANTASY XIV - A Realm Reborn");
        junction::create(&real, &alias).expect("junction");
        assert_eq!(root_for(&alias), None, "安裝資料夾經連結指到影子的位置");
    }

    #[test]
    fn helper_is_not_started_when_shadow_parent_passes_through_links() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 暫存目錄本身可能是短檔名或經過連結：先取實際位置，「不經過連結」的基準才成立。
        let base = PathBuf::from(running::comparable(&fs::canonicalize(dir.path()).expect("canonicalize")));
        let real = base.join("real");
        let name = "FINAL FANTASY XIV - A Realm Reborn";
        fs::create_dir_all(real.join("SquareEnix").join(name)).expect("mkdir");
        // 設定與 HKLM 是同一個字面路徑（helper 以 HKLM 的字面路徑持有影子上層）。
        let start = |install: PathBuf| {
            helper_blocker(&root_for(&install).expect("影子位置"), &install, Some(install.clone()))
        };
        assert_eq!(start(real.join("SquareEnix").join(name)), None, "上層路徑不經過連結：啟動 helper");

        // SquareEnix 搬走後在原處留 junction（上層本身是連結）；或更上層是 junction（路徑經過連結）。
        junction::create(real.join("SquareEnix"), base.join("moved")).expect("junction");
        junction::create(&real, base.join("alias")).expect("junction");
        for install in [base.join("moved").join(name), base.join(r"alias\SquareEnix").join(name)] {
            let reason = start(install.clone()).unwrap_or_else(|| panic!("上層經過連結時不啟動 helper：{}", install.display()));
            assert!(reason.contains("連結"), "原因要寫明是連結造成的：{reason}");
        }
    }

    #[test]
    fn sqpack_names() {
        assert_eq!(sqpack_id("0A0000.win32.DAT0").as_deref(), Some("0a0000"));
        assert_eq!(sqpack_id("000000.win32.dat12").as_deref(), Some("000000"));
        assert_eq!(sqpack_id("000000.win32.index2").as_deref(), Some("000000"));
        for bad in ["000000.win32.dat", "00000.win32.index", "00000g.win32.index", "000000.win32.index3", "000000.dat0"] {
            assert_eq!(sqpack_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn pack_needs_dat0_and_index_for_each_id() {
        let f = Fixture::new();
        let pack = f.read_pack();
        assert_eq!(pack.ids, ["000000"]);
        assert_eq!(pack.files.len(), 3, "readme.txt 不算");

        write(&f.pack().join("0a0000.win32.dat0"), &dat0(2, b"x"));
        let err = read_pack(&f.pack()).expect_err("0a0000 缺 index");
        assert!(err.to_string().contains("0a0000.win32.index"), "{err}");

        let empty = f.dir.path().join("empty");
        fs::create_dir(&empty).expect("mkdir");
        assert!(matches!(read_pack(&empty), Err(AppError::ExternalPackInvalid { .. })));
        assert!(matches!(read_pack(&f.dir.path().join("missing")), Err(AppError::ExternalPackInvalid { .. })));
    }

    /// 包裡名稱像 sqpack 的連結不收：影子以 hardlink 連到包的檔案，連結會連到別處。
    #[test]
    fn pack_ignores_links() {
        let f = Fixture::new();
        let elsewhere = f.dir.path().join("elsewhere");
        fs::create_dir(&elsewhere).expect("mkdir");
        junction::create(&elsewhere, f.pack().join("0a0000.win32.dat0")).expect("junction");
        junction::create(&elsewhere, f.pack().join("0a0000.win32.index")).expect("junction");
        let pack = f.read_pack();
        assert_eq!(pack.ids, ["000000"], "連結不算一組 sqpack");
        assert!(!pack.has("0a0000.win32.dat0"));
    }

    #[test]
    fn fingerprint_decides_current() {
        let f = Fixture::new();
        let pack = f.read_pack();
        assert!(is_current(&pack, &f.install()).expect("check"));
        // 遊戲更新改了官方 dat0 的指紋：包過期。
        write(&sqpack_dir(&f.install()).join("000000.win32.dat0"), &dat0(9, b"patched"));
        assert!(!is_current(&pack, &f.install()).expect("check"));
        // 安裝資料夾沒有同名 dat0：過期。
        fs::remove_file(sqpack_dir(&f.install()).join("000000.win32.dat0")).expect("remove");
        assert!(!is_current(&pack, &f.install()).expect("check"));
    }

    #[test]
    fn truncated_dat0_is_an_error() {
        let f = Fixture::new();
        write(&f.pack().join("000000.win32.dat0"), b"short");
        assert!(is_current(&f.read_pack(), &f.install()).is_err());
    }

    #[test]
    fn shadow_reads_pack_over_install() {
        let f = Fixture::new();
        f.build();
        let game = f.root().join("game");
        let ffxiv = sqpack_dir(&f.root());
        let install_ffxiv = sqpack_dir(&f.install());

        // 檔案都是 hardlink（含執行檔）：和安裝或外部包的檔是同一個檔；資料夾經 junction 讀到安裝資料夾。
        assert!(same_file(&game.join(GAME_EXE), &f.install().join(r"game").join(GAME_EXE)), "執行檔");
        assert!(same_file(&game.join("ffxivgame.ver"), &f.install().join(r"game\ffxivgame.ver")));
        assert!(same_file(&ffxiv.join("0a0000.win32.dat0"), &install_ffxiv.join("0a0000.win32.dat0")));
        assert!(same_file(&ffxiv.join("000000.win32.index"), &f.pack().join("000000.win32.index")), "包取代同名檔");
        assert!(same_file(&ffxiv.join("000000.win32.index2"), &f.pack().join("000000.win32.index2")), "包新增的檔");
        assert_eq!(fs::read(game.join(r"movie\ffxiv\00000.bk2")).expect("movie"), b"movie");
        assert_eq!(fs::read(game.join(r"sqpack\ex1\020100.win32.dat0")).expect("ex1"), b"ex1");
        assert!(junction::exists(game.join("movie")).expect("movie junction"));
        assert!(!ffxiv.join("backup").exists(), "安裝資料夾裡的子資料夾不帶進影子");
        assert!(!ffxiv.join("readme.txt").exists());
        assert!(belongs_to(&f.root(), &f.install()).expect("source"));
    }

    #[test]
    fn rebuild_keeps_install_and_pack() {
        let f = Fixture::new();
        f.build();
        // 安裝更新時多了一個檔：重建後影子也有。
        write(&sqpack_dir(&f.install()).join("0a0000.win32.index"), b"new index");
        f.build();
        assert_eq!(fs::read(sqpack_dir(&f.root()).join("0a0000.win32.index")).expect("new"), b"new index");
        // 刪影子只刪連結：連結指向的檔案與資料夾都還在。
        fs::remove_dir_all(f.root()).expect("remove shadow");
        let game = f.install().join("game");
        assert_eq!(fs::read(game.join(GAME_EXE)).expect("exe"), b"exe");
        assert_eq!(fs::read(game.join(r"movie\ffxiv\00000.bk2")).expect("movie"), b"movie");
        assert_eq!(fs::read(game.join(r"sqpack\ex1\020100.win32.dat0")).expect("ex1"), b"ex1");
        assert_eq!(fs::read(sqpack_dir(&f.install()).join("000000.win32.index")).expect("idx"), b"official index");
        assert_eq!(fs::read(f.pack().join("000000.win32.index")).expect("pack"), b"mod index");
    }

    /// protocols §1.6「是否需要重建」：原地改寫跟著 hardlink 變，不用重建；取代、新增、刪除檔案，換包，
    /// 影子多出項目或屬於別的安裝，都要重建。
    #[test]
    fn freshness_follows_links_not_contents() {
        let f = Fixture::new();
        assert!(!f.fresh(), "沒有影子");
        f.build();
        assert!(f.fresh());

        let ffxiv = sqpack_dir(&f.install());
        let mut file = OpenOptions::new().write(true).open(ffxiv.join("0a0000.win32.dat0")).expect("open");
        file.write_all(b"in place").expect("rewrite");
        drop(file);
        assert!(f.fresh(), "原地改寫");
        assert_eq!(&fs::read(sqpack_dir(&f.root()).join("0a0000.win32.dat0")).expect("read")[..8], b"in place");

        let changes: [(&str, &dyn Fn()); 6] = [
            ("安裝的檔被取代", &|| replace(&ffxiv.join("0a0000.win32.dat0"), &dat0(2, b"replaced"))),
            ("安裝新增檔案", &|| write(&ffxiv.join("0a0000.win32.index"), b"new")),
            ("安裝新增資料夾", &|| write(&f.install().join(r"game\sqpack\ex2\x.dat0"), b"ex2")),
            ("安裝刪除檔案", &|| fs::remove_file(f.install().join(r"game\ffxivgame.ver")).expect("rm")),
            ("外部包的檔被取代", &|| replace(&f.pack().join("000000.win32.index"), b"new mod index")),
            ("影子多出項目", &|| write(&sqpack_dir(&f.root()).join("extra.txt"), b"x")),
        ];
        for (what, change) in changes {
            change();
            assert!(!f.fresh(), "{what}");
            f.build();
            assert!(f.fresh(), "{what} 重建後");
        }

        let other = f.dir.path().join("other");
        fs::create_dir(&other).expect("mkdir");
        fs::write(f.root().join(SOURCE_FILE), other.to_string_lossy().as_bytes()).expect("source");
        assert!(!f.fresh(), "影子屬於別的安裝");
    }

    /// 影子上層一般權限就能修改（測試的暫存資料夾）：在程式內重建，不經 helper；原版遊戲在執行時不重建
    /// （重建會刪掉執行中遊戲開著的檔案的連結）。
    #[test]
    fn rebuild_in_process_when_parent_is_modifiable() {
        let _processes = GAME_PROCESSES.blocking_write();
        let f = Fixture::new();
        let original = crate::game::game_exe(&f.install());
        fs::remove_file(&original).expect("remove exe");
        let game = FakeProcess::start(&original);
        let err = rebuild(&f.root(), &f.install(), &f.read_pack()).expect_err("game running");
        assert!(err.to_string().contains("正在執行"), "{err}");
        assert!(!f.root().exists(), "沒有動到影子");

        drop(game);
        rebuild(&f.root(), &f.install(), &f.read_pack()).expect("rebuild");
        assert!(f.fresh());
    }

    #[test]
    fn launch_exe_follows_settings_and_fingerprint() {
        let _processes = GAME_PROCESSES.blocking_read();
        let f = Fixture::new();
        let original = crate::game::game_exe(&f.install());
        let shadow = game_exe(&f.root());
        let rebuilds = Cell::new(0);
        let launch = |settings: &ExternalPack| {
            launch_exe(settings, &f.install(), |root, pack| {
                rebuilds.set(rebuilds.get() + 1);
                build(root, &f.install(), &plan(&f.install(), pack)?, |dir| fs::create_dir(dir))
            })
        };

        // 關閉、未確認風險、沒有資料夾：原執行檔、不提示、不建影子。
        let off = [
            ExternalPack { enabled: false, ..f.settings() },
            ExternalPack { risk_accepted: false, ..f.settings() },
            ExternalPack { dir: None, ..f.settings() },
        ];
        for settings in off {
            assert_eq!(launch(&settings), LaunchExe { exe: original.clone(), warning: None });
            assert!(!f.root().exists());
        }
        assert_eq!(rebuilds.get(), 0);

        // 指紋一致：第一次重建並從影子啟動；影子沒變就直接沿用，不再重建。
        assert_eq!(launch(&f.settings()), LaunchExe { exe: shadow.clone(), warning: None });
        assert_eq!(rebuilds.get(), 1);
        assert_eq!(launch(&f.settings()), LaunchExe { exe: shadow.clone(), warning: None });
        assert_eq!(rebuilds.get(), 1, "fresh 時不重建");

        // 重建失敗（例如拒絕 UAC）：原執行檔＋原因。
        replace(&f.install().join(r"game\ffxivgame.ver"), b"2026.10.17.0000.0000");
        let failed = launch_exe(&f.settings(), &f.install(), |_, _| Err(shadow_error("沒有同意系統管理員權限")));
        assert_eq!(failed.exe, original);
        assert!(failed.warning.as_deref().is_some_and(|w| w.contains("沒有同意系統管理員權限")), "{failed:?}");

        // 指紋不一致：原執行檔＋過期提示，不重建。
        write(&sqpack_dir(&f.install()).join("000000.win32.dat0"), &dat0(9, b"patched"));
        let expired = launch(&f.settings());
        assert_eq!(expired.exe, original);
        assert!(expired.warning.as_deref().is_some_and(|w| w.contains("外部包已過期")), "{expired:?}");

        // 外部包資料夾被移走：原執行檔＋原因。
        fs::rename(f.pack(), f.dir.path().join("moved")).expect("move pack");
        let missing = launch(&f.settings());
        assert_eq!(missing.exe, original);
        assert!(missing.warning.is_some());
        assert_eq!(rebuilds.get(), 1);
    }

    /// 影子裡建好之後才放進去的檔案（例如工具寫在遊戲執行檔旁的截圖）：不重建（重建會連它一起刪掉），用原執行檔
    /// 並列出路徑；移走之後照常重建。連結不算（刪除只刪連結本身）。
    #[test]
    fn files_added_after_build_block_rebuild() {
        let _processes = GAME_PROCESSES.blocking_read();
        let f = Fixture::new();
        f.build();
        // 建立時間要晚於 `source`：檔案系統時間的精度約 16 ms。
        std::thread::sleep(std::time::Duration::from_millis(50));
        let added = f.root().join(r"game\screenshot.png");
        write(&added, b"user data");
        let at_root = f.root().join("notes.txt");
        write(&at_root, b"user data");
        junction::create(f.dir.path(), f.root().join(r"game\linked")).expect("junction");

        let blocked = launch_exe(&f.settings(), &f.install(), |_, _| panic!("有建好之後才放進去的檔案時不重建"));
        assert_eq!(blocked.exe, crate::game::game_exe(&f.install()));
        let warning = blocked.warning.expect("warning");
        assert!(warning.contains("screenshot.png") && warning.contains("notes.txt"), "{warning}");
        assert!(!warning.contains("linked") && !warning.contains("source"), "{warning}");
        assert!(added.is_file() && at_root.is_file(), "檔案還在");

        fs::remove_file(&added).expect("move away");
        fs::remove_file(&at_root).expect("move away");
        let rebuilds = Cell::new(0);
        let launch = launch_exe(&f.settings(), &f.install(), |root, pack| {
            rebuilds.set(rebuilds.get() + 1);
            build(root, &f.install(), &plan(&f.install(), pack)?, |dir| fs::create_dir(dir))
        });
        assert_eq!(launch, LaunchExe { exe: game_exe(&f.root()), warning: None });
        assert_eq!(rebuilds.get(), 1, "只剩連結，照常重建");
        assert!(f.fresh());
    }

    /// 影子的遊戲在執行時不重建（重建會刪掉執行中的檔案與連結），沿用現有的影子；這個安裝也算使用中，
    /// 不能更新（protocols §3 例外）。
    #[test]
    fn running_shadow_is_reused_and_blocks_update() {
        let _processes = GAME_PROCESSES.blocking_write();
        let f = Fixture::new();
        f.build();
        let shadow = game_exe(&f.root());
        fs::remove_file(&shadow).expect("remove linked exe");
        let _game = FakeProcess::start(&shadow);
        assert!(!f.fresh(), "執行檔換成替身，影子不是最新");
        let launch = launch_exe(&f.settings(), &f.install(), |_, _| panic!("執行中不重建"));
        assert_eq!(launch, LaunchExe { exe: shadow, warning: None });

        assert!(running::install_in_use(&f.install()).expect("in use"), "影子在安裝的上一層");
        let original = crate::game::game_exe(&f.install());
        assert!(!running::exe_running(&original).expect("original"), "原執行檔沒有在跑");
        let other = f.dir.path().join("other");
        fs::create_dir(&other).expect("mkdir");
        assert!(!running::install_in_use(&other).expect("other"), "影子不屬於別的安裝");
    }

    #[test]
    fn exe_of_matches_source_install() {
        let f = Fixture::new();
        assert_eq!(exe_of(&f.root(), &f.install()), None, "沒有影子");
        f.build();
        assert_eq!(exe_of(&f.root(), &f.install()), Some(game_exe(&f.root())));
        let other = f.dir.path().join("other");
        fs::create_dir(&other).expect("mkdir");
        assert_eq!(exe_of(&f.root(), &other), None, "別的安裝");
        // 沒建完（沒有 source）但執行檔在：任何安裝都保守算屬於。
        fs::remove_file(f.root().join(SOURCE_FILE)).expect("remove source");
        assert_eq!(exe_of(&f.root(), &other), Some(game_exe(&f.root())));
        assert!(!belongs_to(&f.root(), &f.install()).expect("belongs"), "啟動時不沿用來歷不明的影子");
    }

    #[test]
    fn pack_location_must_be_absolute_and_apart() {
        let f = Fixture::new();
        let check = |dir: &Path| check_dir(dir, Some(&f.install()));
        let copy_pack = |to: &Path| {
            for name in ["000000.win32.dat0", "000000.win32.index"] {
                write(&to.join(name), &fs::read(f.pack().join(name)).expect("read pack"));
            }
        };
        let overlap = |dir: &Path| {
            let err = check(dir).expect_err("overlap");
            assert!(err.to_string().contains("不能放在"), "{}: {err}", dir.display());
        };
        assert!(check(&f.pack()).is_ok());
        assert!(matches!(check(Path::new("pack")), Err(AppError::ExternalPackInvalid { .. })), "相對路徑");
        // 含 `..` 的路徑實際指到合格的包仍拒絕：helper 不收這種參數，設定時就要擋。
        let dotted = f.pack().join("..").join(f.pack().file_name().expect("name"));
        assert!(matches!(check(&dotted), Err(AppError::ExternalPackInvalid { .. })), "含 ..");

        // 在影子資料夾裡（重建時會被刪）、包含影子與安裝資料夾、在安裝資料夾裡、就是安裝資料夾：都拒絕，
        // 即使裡面的包本身合格。
        let in_root = f.root().join("pack");
        copy_pack(&in_root);
        overlap(&in_root);
        copy_pack(f.dir.path());
        overlap(f.dir.path());
        overlap(&sqpack_dir(&f.install()));
        overlap(&f.install());
        // 大小寫不同仍是同一個資料夾；只是名稱前綴相同不算重疊。
        overlap(&PathBuf::from(f.install().to_string_lossy().to_uppercase()).join("game"));
        let sibling = f.dir.path().join("FINAL FANTASY XIV - A Realm Reborn pack");
        copy_pack(&sibling);
        assert!(check(&sibling).is_ok());
        // 還沒設定安裝資料夾：只檢查路徑與內容。
        assert!(check_dir(&in_root, None).is_ok());

        // 啟動時也檢查：包在影子資料夾裡就不重建，包不會被刪。
        let settings = ExternalPack { dir: Some(in_root.clone()), ..f.settings() };
        let launch = launch_exe(&settings, &f.install(), |_, _| panic!("不合格的包不重建"));
        assert_eq!(launch.exe, crate::game::game_exe(&f.install()));
        assert!(launch.warning.is_some_and(|w| w.contains("不能放在")));
        assert!(in_root.join("000000.win32.index").is_file());
    }

    #[test]
    fn enabling_needs_dir_and_risk_confirmed_once() {
        let dir = Some(PathBuf::from(r"D:\pack"));
        let mut pack = ExternalPack::default();
        assert!(matches!(
            apply_settings(&mut pack, dir.clone(), true, false),
            Err(AppError::ExternalPackRiskNotAccepted)
        ));
        assert_eq!(pack, ExternalPack::default(), "被拒時不改設定");
        assert!(matches!(apply_settings(&mut pack, None, true, true), Err(AppError::ExternalPackInvalid { .. })));

        // 只填資料夾不開啟：不用確認。
        apply_settings(&mut pack, dir.clone(), false, false).expect("save dir");
        assert_eq!(pack, ExternalPack { dir: dir.clone(), enabled: false, risk_accepted: false });

        apply_settings(&mut pack, dir.clone(), true, true).expect("enable");
        assert_eq!(pack, ExternalPack { dir: dir.clone(), enabled: true, risk_accepted: true });
        // 確認只記一次：關掉再開不用再確認，清空資料夾也保留確認。
        apply_settings(&mut pack, None, false, false).expect("disable");
        assert!(pack.risk_accepted && !pack.enabled && pack.dir.is_none());
        apply_settings(&mut pack, dir.clone(), true, false).expect("re-enable");
        assert!(pack.enabled && pack.risk_accepted);
    }
}
