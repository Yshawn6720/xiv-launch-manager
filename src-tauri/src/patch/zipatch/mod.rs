//! ZiPatch 解析與套用（protocols §5.4），以及套用成功後寫 `.ver`（§5.1）。
//!
//! 兩階段，呼叫端全程用同一個已開啟的 handle（以拒絕共用寫入開啟、已用
//! `patch::list::verify_blocks` 核對；兩個階段都會自行 seek 回檔頭）：
//! 1. [`scan_patch`]：讀完整個 patch，做完 §5.4 所有只依內容的檢查（magic、每個 chunk 的
//!    CRC32、欄位長度、chunk 與子命令種類、Deflate 解碼與長度、輸出路徑正規化），並確認既有
//!    的輸出路徑沒有 junction／symlink。期間不寫任何檔案：失敗＝遊戲目錄未被寫過。
//! 2. [`ScannedPatch::apply`]：從檔頭再讀一次並逐 chunk 套用，每次寫入前再檢查 junction／
//!    symlink；遊戲檔以不共用方式開啟並持有到最後 `sync_all`。失敗＝遊戲目錄可能已被寫入（只有本
//!    patch 尚未改動遊戲目錄時的共用違規例外，見該函式）。
//!
//! 錯誤只帶規則與偏移，不回顯 patch 內容（路徑、資料）。

mod chunk;
mod target;

pub(crate) use target::is_dos_device;

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::num::NonZeroU32;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::ERROR_SHARING_VIOLATION;

use crate::error::{AppError, AppResult};
use crate::game::MAX_EXPANSION;
use crate::patch::apply::ApplyFailure;
use crate::patch::list::Repo;
use crate::store::replace_write_through;
use chunk::{Op, units};

/// 檔頭 magic（§5.4.2）。
const MAGIC: [u8; 12] = [
    0x91, b'Z', b'I', b'P', b'A', b'T', b'C', b'H', b'\r', b'\n', 0x1a, b'\n',
];
/// chunk 外框：`size`、`type`、CRC32 各 4 bytes。
const CHUNK_FRAME: u64 = 12;
/// `size` 的上限 2^31−1（XL `checked((int)size)`）。
const MAX_CHUNK_SIZE: u32 = i32::MAX as u32;
const READ_BUF_SIZE: usize = 64 * 1024;
/// SQPK/D、E 的空區塊標頭長度。
const EMPTY_BLOCK_HEADER_LEN: usize = 24;
/// 寫 0 時的來源緩衝區。
static ZEROS: [u8; 64 * 1024] = [0; 64 * 1024];

/// 已通過 [`scan_patch`] 的 patch，持有掃描時的同一個 handle；只能用它套用。
pub struct ScannedPatch<'a, R> {
    reader: &'a mut R,
    root: PathBuf,
    total: u64,
}

/// 掃描整個 patch：§5.4 只依內容的檢查全部做完，並確認既有的輸出路徑沒有 junction／symlink。
/// 不寫入任何檔案，失敗時遊戲目錄未被寫過。`game_root` 一律為 `{install}\game`
/// （game 與 exN repo 相同，§5.4.5），必須是既有目錄。
pub fn scan_patch<'a, R: Read + Seek>(
    patch: &'a mut R,
    game_root: &Path,
) -> AppResult<ScannedPatch<'a, R>> {
    let total = scan(patch, game_root)?;
    Ok(ScannedPatch {
        reader: patch,
        root: game_root.to_path_buf(),
        total,
    })
}

impl<R: Read + Seek> ScannedPatch<'_, R> {
    /// patch 總長（bytes），即進度的終點。
    pub fn total_bytes(&self) -> u64 {
        self.total
    }

    /// 從檔頭逐 chunk 套用。每套完一個 chunk 就以「已處理的 patch bytes」呼叫 `progress`，
    /// 最後一次等於 [`Self::total_bytes`]；之後對本次寫入過的每個檔案 `sync_all` 才回傳成功，
    /// 所以成功回傳時資料已落盤，呼叫端接著寫 `.ver` 不會比資料先落盤。
    ///
    /// 遊戲檔以不共用（share mode 0）開啟，寫入與 `sync_all` 用同一個 handle，持有到落盤完成：套用期間
    /// 其他程序（含遊戲）開不了、刪不掉、改不了名已寫過的檔。失敗分類（protocols §5.4.1、主 note
    /// 「遊戲執行判定」）：本 patch 尚未改動遊戲目錄時遇共用違規＝[`ApplyFailure::BeforeWrite`]
    /// （[`AppError::GameRunning`]）；其他失敗一律 [`ApplyFailure::AfterWrite`]——遊戲目錄可能已被寫入，
    /// 呼叫端停止、不寫該 patch 的 `.ver`、保留 dirty 標記。
    pub fn apply(self, progress: &mut dyn FnMut(u64)) -> Result<(), ApplyFailure> {
        let mut writer = Writer {
            root: self.root,
            open: HashMap::new(),
            touched: false,
        };
        writer
            .run(self.reader, progress)
            .map_err(|fail| writer.classify(fail))
    }
}

/// 把 `version_id` 原樣寫入 `repo` 的 `.ver`（protocols §5.1 表：game＝`{root}\ffxivgame.ver`、
/// exN＝`{root}\sqpack\exN\exN.ver`；`game_root` 同套用根目錄 `{install}\game`）。先寫同目錄
/// 暫存檔並 `sync_all`，再以 [`replace_write_through`] 取代舊檔（改名寫入磁碟後才返回）；不建立目錄。
///
/// 只能在該 patch 的 [`ScannedPatch::apply`] 回傳成功後呼叫：掃描或套用失敗的 patch 不寫，
/// 版本逐 patch 推進（前面成功的保留，失敗與未開始的不推進）。
///
/// 這個寫穿改名是該 patch 的 commit 點：在它之前 [`ScannedPatch::apply`] 已對每個寫過的遊戲檔
/// `sync_all`，parent 在它成功之後才清 dirty 標記。新建的遊戲檔不另 flush 所在目錄：NTFS 的 metadata
/// 變更依序寫進同一份日誌，寫穿改名落盤時，排在它之前的建檔紀錄也已落盤。
pub fn write_version(game_root: &Path, repo: Repo, version_id: &str) -> AppResult<()> {
    let rel = version_file(repo)?;
    target::ensure_no_link(game_root, &rel)?;
    let file = game_root.join(&rel);
    let tmp = file.with_file_name(format!("xlm-{}.ver.tmp", uuid::Uuid::new_v4().simple()));
    let created = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| AppError::io("建立 .ver 暫存檔", e))?;
    commit_version(created, &tmp, &file, version_id.as_bytes()).map_err(|(context, err)| {
        match fs::remove_file(&tmp) {
            Ok(()) => AppError::io(context, err),
            Err(cleanup) => AppError::Io {
                context: format!("{context}（暫存檔也無法刪除：{cleanup}）"),
                message: err.to_string(),
            },
        }
    })
}

/// 違反的 §5.4 規則。訊息只描述規則，不含 patch 內容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    Magic,
    Truncated,
    MissingEof,
    TrailingData,
    ChunkTooLarge,
    Crc,
    UnknownChunk,
    Short,
    SqpkLength,
    UnknownSqpk,
    Platform,
    Expansion,
    BlockCount,
    HeaderKind,
    RemoveAll,
    FileOperation,
    FileOffset,
    FileSize,
    BlockHeader,
    BlockSize,
    BlockBounds,
    Deflate,
    DecompressedLength,
    NonAscii,
    UnsafePath,
    Link,
    Root,
    Repo,
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Rule::Magic => "檔頭 magic 不符",
            Rule::Truncated => "chunk 超出檔尾",
            Rule::MissingEof => "到檔尾仍未出現 EOF_",
            Rule::TrailingData => "EOF_ 之後還有資料",
            Rule::ChunkTooLarge => "chunk 長度超過 2^31−1",
            Rule::Crc => "chunk CRC32 不符",
            Rule::UnknownChunk => "不支援的 chunk 種類",
            Rule::Short => "欄位或資料超出 chunk body",
            Rule::SqpkLength => "SQPK 內部長度與 chunk 長度不符",
            Rule::UnknownSqpk => "不支援的 SQPK 子命令",
            Rule::Platform => "SQPK/T 的 platform 不是 Win32",
            Rule::Expansion => "SqPack 目標的 ExpansionId 大於 5",
            Rule::BlockCount => "SQPK/D、E 的 BlockCount 為 0",
            Rule::HeaderKind => "SQPK/H 的檔案種類或標頭種類不支援",
            Rule::RemoveAll => "不支援 SQPK/F RemoveAll",
            Rule::FileOperation => "不支援的 SQPK/F operation",
            Rule::FileOffset => "SQPK/F 的 FileOffset 為負",
            Rule::FileSize => "SQPK/F 的 FileSize 與資料區塊解出長度總和不符",
            Rule::BlockHeader => "資料區塊的 HeaderSize 不是 16",
            Rule::BlockSize => "資料區塊的長度欄位為負",
            Rule::BlockBounds => "資料區塊沒有正好結束於子命令結尾",
            Rule::Deflate => "資料區塊 Deflate 解碼失敗",
            Rule::DecompressedLength => "資料區塊解出長度與 DecompressedSize 不符",
            Rule::NonAscii => "字串含 0x20–0x7E 以外的字元",
            Rule::UnsafePath => {
                "輸出路徑不安全（磁碟代號、ADS、..、空成分、裝置名或指向根目錄）"
            }
            Rule::Link => "輸出路徑經過 junction／symlink",
            Rule::Root => "套用根目錄不存在或不是目錄",
            Rule::Repo => "repo 不是 game 或 ex1–ex5",
        })
    }
}

/// 掃描或套用失敗的原因。
#[derive(Debug)]
enum Fail {
    /// 違反 §5.4；`offset` 為所在 chunk 在 patch 檔中的起點（與 chunk 無關時為 `None`）。
    Reject { offset: Option<u64>, rule: Rule },
    Io {
        context: &'static str,
        source: io::Error,
    },
}

impl Fail {
    fn reject(rule: Rule) -> Self {
        Fail::Reject { offset: None, rule }
    }

    fn io(context: &'static str, source: io::Error) -> Self {
        Fail::Io { context, source }
    }

    /// 檔案被其他 handle 以不相容的共用模式開著（`ERROR_SHARING_VIOLATION`）。
    fn is_sharing_violation(&self) -> bool {
        matches!(self, Fail::Io { source, .. }
            if source.raw_os_error() == Some(ERROR_SHARING_VIOLATION.0 as i32))
    }

    /// 補上 chunk 偏移（已有偏移或 I/O 錯誤則不變）。
    fn at(self, offset: u64) -> Self {
        match self {
            Fail::Reject { offset: None, rule } => Fail::Reject {
                offset: Some(offset),
                rule,
            },
            other => other,
        }
    }
}

impl From<Fail> for AppError {
    fn from(fail: Fail) -> Self {
        match fail {
            Fail::Reject {
                offset: Some(offset),
                rule,
            } => AppError::PatchRejected {
                reason: format!("偏移 {offset}：{rule}"),
            },
            Fail::Reject { offset: None, rule } => AppError::PatchRejected {
                reason: rule.to_string(),
            },
            Fail::Io { context, source } => AppError::io(context, source),
        }
    }
}

/// 第一階段：只讀。F/A 的資料區塊實際解碼一次（丟棄輸出）以驗 Deflate 與長度；每個輸出
/// 路徑檢查一次 junction／symlink。
fn scan<R: Read + Seek>(reader: &mut R, root: &Path) -> Result<u64, Fail> {
    target::ensure_root(root)?;
    let mut checked: HashSet<PathBuf> = HashSet::new();
    walk(reader, |_, op| {
        if let Op::AddFile { blocks, .. } = &op {
            for block in blocks {
                block.decode(&mut discard)?;
            }
        }
        if let Some(rel) = op.target()
            && !checked.contains(rel)
        {
            target::ensure_no_link(root, rel)?;
            checked.insert(rel.to_path_buf());
        }
        Ok(())
    })
}

fn discard(_: &[u8]) -> Result<(), Fail> {
    Ok(())
}

/// 從檔頭依序讀出每個 chunk：驗外框與 CRC32、解析成 [`Op`] 後交給 `visit(chunk 結尾偏移, op)`，
/// 讀完 `EOF_` 為止，且 `EOF_` 必須正好結束於檔尾（§5.4.2）。回傳 patch 總長。
fn walk<R: Read + Seek>(
    reader: &mut R,
    mut visit: impl FnMut(u64, Op<'_>) -> Result<(), Fail>,
) -> Result<u64, Fail> {
    let len = reader.seek(SeekFrom::End(0)).map_err(read_error)?;
    reader.rewind().map_err(read_error)?;
    let mut reader = BufReader::with_capacity(READ_BUF_SIZE, reader);

    let mut magic = [0u8; MAGIC.len()];
    if len < magic.len() as u64 {
        return Err(Fail::reject(Rule::Magic).at(0));
    }
    reader.read_exact(&mut magic).map_err(read_error)?;
    if magic != MAGIC {
        return Err(Fail::reject(Rule::Magic).at(0));
    }

    let mut offset = MAGIC.len() as u64;
    let mut body = Vec::new();
    loop {
        if offset == len {
            return Err(Fail::reject(Rule::MissingEof).at(offset));
        }
        if len - offset < CHUNK_FRAME {
            return Err(Fail::reject(Rule::Truncated).at(offset));
        }
        let mut head = [0u8; 8];
        reader.read_exact(&mut head).map_err(read_error)?;
        let [s0, s1, s2, s3, k0, k1, k2, k3] = head;
        let size = u32::from_be_bytes([s0, s1, s2, s3]);
        let kind = [k0, k1, k2, k3];
        if size > MAX_CHUNK_SIZE {
            return Err(Fail::reject(Rule::ChunkTooLarge).at(offset));
        }
        let end = offset + CHUNK_FRAME + u64::from(size);
        if end > len {
            return Err(Fail::reject(Rule::Truncated).at(offset));
        }
        body.resize(size as usize, 0);
        reader.read_exact(&mut body).map_err(read_error)?;
        let mut crc = [0u8; 4];
        reader.read_exact(&mut crc).map_err(read_error)?;
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&kind);
        hasher.update(&body);
        if hasher.finalize() != u32::from_be_bytes(crc) {
            return Err(Fail::reject(Rule::Crc).at(offset));
        }

        let op = chunk::parse(kind, &body).map_err(|rule| Fail::reject(rule).at(offset))?;
        let last = matches!(op, Op::End);
        visit(end, op).map_err(|fail| fail.at(offset))?;
        offset = end;
        if last {
            return if offset == len {
                Ok(len)
            } else {
                Err(Fail::reject(Rule::TrailingData).at(offset))
            };
        }
    }
}

fn read_error(e: io::Error) -> Fail {
    Fail::io("讀取 patch 檔", e)
}

fn write_error(e: io::Error) -> Fail {
    Fail::io("寫入遊戲檔", e)
}

/// 第二階段的寫入狀態。
struct Writer {
    root: PathBuf,
    /// 本 patch 開著的遊戲檔（不共用），持有到 [`Writer::run`] 最後 `sync_all` 完成。key 見 [`file_key`]：
    /// 同一個檔不同大小寫的拼法共用一個 handle。以其他別名（如 8.3 短檔名）再開同一個檔會撞上自己的
    /// handle 而共用違規，依已寫入處理。
    open: HashMap<String, File>,
    /// 本 patch 是否可能已改動遊戲目錄：開過遊戲檔（可能是新建）、建立或刪除了目錄、刪了檔。
    touched: bool,
}

impl Writer {
    fn run<R: Read + Seek>(
        &mut self,
        reader: &mut R,
        progress: &mut dyn FnMut(u64),
    ) -> Result<(), Fail> {
        target::ensure_root(&self.root)?;
        walk(reader, |end, op| {
            self.apply_op(op)?;
            #[cfg(test)]
            test_support::op_applied();
            progress(end);
            Ok(())
        })?;
        // 用寫入的同一個 handle 落盤，不依路徑重開：寫完到落盤之間其他程序無從刪除或替換。本 patch
        // 刪掉的檔已在 SQPK/F `D` 時移出 `open`，不需落盤；這裡不會遇到「檔案不存在」。
        for file in self.open.values() {
            file.sync_all()
                .map_err(|e| Fail::io("寫入遊戲檔到磁碟", e))?;
        }
        Ok(())
    }

    /// 本 patch 尚未改動遊戲目錄時遇共用違規＝寫入前中止，視同遊戲執行中；其他一律已寫入。
    fn classify(&self, fail: Fail) -> ApplyFailure {
        if !self.touched && fail.is_sharing_violation() {
            ApplyFailure::BeforeWrite(AppError::GameRunning)
        } else {
            ApplyFailure::AfterWrite(fail.into())
        }
    }

    /// 套用一個動作（§5.4.3–§5.4.4）。寫入前再檢查一次 junction／symlink。
    fn apply_op(&mut self, op: Op<'_>) -> Result<(), Fail> {
        if let Some(rel) = op.target() {
            target::ensure_no_link(&self.root, rel)?;
        }
        match op {
            Op::Skip | Op::End => Ok(()),
            Op::MakeDir(rel) => {
                self.touched = true;
                fs::create_dir_all(self.root.join(rel)).map_err(|e| Fail::io("建立目錄", e))
            }
            Op::RemoveDir(rel) => {
                fs::remove_dir(self.root.join(rel)).map_err(|e| Fail::io("刪除目錄", e))?;
                self.touched = true;
                Ok(())
            }
            Op::Write {
                file,
                offset,
                data,
                zeros,
            } => {
                let out = self.open_for_write(&file)?;
                seek_to(out, offset)?;
                out.write_all(data).map_err(write_error)?;
                write_zeros(out, zeros)
            }
            Op::EmptyBlocks {
                file,
                offset,
                count,
            } => {
                let out = self.open_for_write(&file)?;
                seek_to(out, offset)?;
                write_zeros(out, units(count.get()))?;
                seek_to(out, offset)?;
                out.write_all(&empty_block_header(count))
                    .map_err(write_error)
            }
            Op::AddFile {
                file,
                offset,
                blocks,
            } => {
                let path = self.root.join(&file);
                if let Some(parent) = path.parent()
                    && !parent.is_dir()
                {
                    self.touched = true;
                    fs::create_dir_all(parent).map_err(|e| Fail::io("建立目錄", e))?;
                }
                let out = self.open_for_write(&file)?;
                if offset == 0 {
                    out.set_len(0).map_err(write_error)?;
                }
                seek_to(out, offset)?;
                for block in &blocks {
                    block.decode(&mut |bytes: &[u8]| out.write_all(bytes).map_err(write_error))?;
                }
                Ok(())
            }
            Op::DeleteFile(rel) => {
                // 本 patch 開著的同一個檔先關掉（要刪了，不需落盤），否則自己的不共用 handle 會擋住刪除。
                self.open.remove(&file_key(&rel));
                match fs::remove_file(self.root.join(rel)) {
                    Ok(()) => {
                        self.touched = true;
                        Ok(())
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(Fail::io("刪除遊戲檔", e)),
                }
            }
        }
    }

    /// 本 patch 對 `rel` 的寫入 handle。第一次用到時開啟既有檔或建立新檔，不截斷、不共用（share mode 0）；
    /// 上層目錄不存在即失敗（SQPK/A、D、E、H 不建目錄）。
    fn open_for_write(&mut self, rel: &Path) -> Result<&mut File, Fail> {
        match self.open.entry(file_key(rel)) {
            Entry::Occupied(entry) => Ok(entry.into_mut()),
            Entry::Vacant(entry) => {
                let file = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .share_mode(0)
                    .open(self.root.join(rel))
                    .map_err(|e| Fail::io("開啟遊戲檔", e))?;
                self.touched = true;
                Ok(entry.insert(file))
            }
        }
    }
}

/// [`Writer::open`] 的 key：ASCII 小寫的相對路徑。NTFS 不分大小寫，輸出路徑已限 ASCII（§5.4.1），
/// 分隔符已由路徑正規化統一（§5.4.5）。
fn file_key(rel: &Path) -> String {
    rel.to_string_lossy().to_ascii_lowercase()
}

fn seek_to(file: &mut File, offset: u64) -> Result<(), Fail> {
    file.seek(SeekFrom::Start(offset)).map_err(write_error)?;
    Ok(())
}

fn write_zeros(file: &mut File, mut len: u64) -> Result<(), Fail> {
    while len > 0 {
        let n = len.min(ZEROS.len() as u64) as usize;
        file.write_all(&ZEROS[..n]).map_err(write_error)?;
        len -= n as u64;
    }
    Ok(())
}

/// SQPK/D、E 的空區塊標頭（皆 LE）：`+0` 128、`+12` u64 `BlockCount − 1`，其餘為 0。
fn empty_block_header(count: NonZeroU32) -> [u8; EMPTY_BLOCK_HEADER_LEN] {
    let mut header = [0u8; EMPTY_BLOCK_HEADER_LEN];
    header[..4].copy_from_slice(&128u32.to_le_bytes());
    header[12..20].copy_from_slice(&u64::from(count.get() - 1).to_le_bytes());
    header
}

/// `.ver` 相對套用根目錄的路徑（protocols §5.1 表）。
fn version_file(repo: Repo) -> Result<PathBuf, Fail> {
    match repo {
        Repo::Game => Ok(PathBuf::from("ffxivgame.ver")),
        Repo::Expansion(n @ 1..=MAX_EXPANSION) => {
            let name = format!("ex{n}");
            Ok(PathBuf::from("sqpack")
                .join(&name)
                .join(format!("{name}.ver")))
        }
        Repo::Expansion(_) => Err(Fail::reject(Rule::Repo)),
    }
}

/// 寫暫存檔、`sync_all`，再以 [`replace_write_through`] 取代 `.ver`；失敗時回傳步驟名與錯誤，由呼叫端
/// 清掉暫存檔。
fn commit_version(
    mut created: File,
    tmp: &Path,
    file: &Path,
    bytes: &[u8],
) -> Result<(), (&'static str, io::Error)> {
    created
        .write_all(bytes)
        .map_err(|e| ("寫入 .ver 暫存檔", e))?;
    created.sync_all().map_err(|e| ("寫入 .ver 暫存檔", e))?;
    drop(created);
    replace_write_through(tmp, file).map_err(|e| ("取代 .ver", e))
}

#[cfg(test)]
mod tests;

/// 測試限定（整個模組只在 `cfg(test)` 編譯，正式建置沒有同步點）：套用執行緒的同步點，以及
/// 其他模組測試用的合成 patch builder。
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::OnceLock;

    pub(crate) use super::tests::{Patch, add_data, junction};

    static OP_APPLIED: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

    /// 設定同步點：[`ScannedPatch::apply`](super::ScannedPatch::apply) 每套完一個動作（已寫入遊戲目錄、
    /// 尚未回報進度，更早於該 patch 的 `sync_all` 與 `.ver`）就在套用執行緒上呼叫 `hook`。作用於整個程序且
    /// 只能設定一次，所以只給以子程序單獨重跑的測試設定（`launch` 的套用中殺程序測試），不可在一般測試
    /// 程序中設定。
    pub(crate) fn on_op_applied(hook: impl Fn() + Send + Sync + 'static) {
        assert!(OP_APPLIED.set(Box::new(hook)).is_ok(), "同步點只能設定一次");
    }

    pub(super) fn op_applied() {
        if let Some(hook) = OP_APPLIED.get() {
            hook();
        }
    }
}
