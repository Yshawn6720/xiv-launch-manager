//! ZiPatch 測試：實際台服 ex1 patch；以 builder 組出的合成 patch（每種 chunk／SQPK 子命令的
//! 套用結果、protocols §5.4 各條拒絕規則）；套用中的共用；輸出路徑；`.ver`。
//!
//! 掃描階段的拒絕一律經 `rejects`／`scan_rule`，同時斷言整個 tempdir 未被寫入。

use std::fs;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use flate2::Compression;
use flate2::write::DeflateEncoder;
use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;

use super::target::{SqpackTarget, relative_path};
use super::{Fail, MAGIC, Rule, scan, scan_patch, write_version};
use crate::error::AppError;
use crate::game;
use crate::patch::apply::ApplyFailure;
use crate::patch::list::Repo;

/// 台服 ex1 2026.08.07 的實際 patch（52,848 bytes；protocols §5.4 的樣本）。內容：FHDR、
/// APLY×2、SQPK X、T、A、H（D＋V）、H（D＋D）、EOF_。
const TW_EX1_PATCH: &[u8] = include_bytes!("../../testdata/D2026.08.07.0000.0000.patch");

// ── fixture ─────────────────────────────────────────────────────────────────

/// tempdir 下的 `game` 為套用根目錄，預先建好 `sqpack\ffxiv` 與 `sqpack\ex1`
/// （SQPK A／D／E／H 不建目錄，實際安裝一定有）。
struct Game {
    dir: tempfile::TempDir,
}

impl Game {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        for folder in ["ffxiv", "ex1"] {
            fs::create_dir_all(dir.path().join("game").join("sqpack").join(folder))
                .expect("mkdir");
        }
        Self { dir }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("game")
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root().join(rel)
    }

    /// 根目錄外、與根目錄同層的目錄（junction 的目標）。
    fn outside(&self) -> PathBuf {
        let outside = self.dir.path().join("outside");
        fs::create_dir_all(&outside).expect("mkdir outside");
        outside
    }

    fn snapshot(&self) -> Vec<Entry> {
        let mut entries = Vec::new();
        collect(self.dir.path(), self.dir.path(), &mut entries);
        entries.sort();
        entries
    }
}

/// 整個 tempdir 的狀態；junction／symlink 只記錄本身，不跟隨。
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Entry {
    Dir(PathBuf),
    File(PathBuf, Vec<u8>),
    Link(PathBuf),
}

fn collect(base: &Path, dir: &Path, out: &mut Vec<Entry>) {
    for entry in fs::read_dir(dir).expect("read_dir") {
        let path = entry.expect("entry").path();
        let rel = path.strip_prefix(base).expect("prefix").to_path_buf();
        let meta = fs::symlink_metadata(&path).expect("metadata");
        if meta.file_type().is_symlink() {
            out.push(Entry::Link(rel));
        } else if meta.is_dir() {
            out.push(Entry::Dir(rel));
            collect(base, &path, out);
        } else {
            out.push(Entry::File(rel, fs::read(path).expect("read")));
        }
    }
}

/// tempdir 內所有檔案（不讀內容），相對 tempdir、排序。
fn list_files(base: &Path) -> Vec<PathBuf> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            let meta = fs::symlink_metadata(&path).expect("metadata");
            if meta.is_dir() && !meta.file_type().is_symlink() {
                walk(base, &path, out);
            } else {
                out.push(path.strip_prefix(base).expect("prefix").to_path_buf());
            }
        }
    }
    let mut files = Vec::new();
    walk(base, base, &mut files);
    files.sort();
    files
}

fn read_at(path: &Path, offset: u64, len: usize) -> Vec<u8> {
    let mut file = fs::File::open(path).expect("open");
    file.seek(SeekFrom::Start(offset)).expect("seek");
    let mut buf = vec![0; len];
    file.read_exact(&mut buf).expect("read");
    buf
}

/// `mklink` 會把路徑裡的 `/` 當成參數開頭，先以 `components` 重組成全 `\` 的路徑。
pub(crate) fn junction(link: &Path, target: &Path) {
    let backslashed = |path: &Path| path.components().collect::<PathBuf>();
    let status = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(backslashed(link))
        .arg(backslashed(target))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("執行 mklink");
    assert!(status.success(), "mklink /J 失敗");
}

/// 掃描必須失敗於某條規則，且整個 tempdir 不變。
fn scan_rule(game: &Game, patch: &[u8]) -> Rule {
    let before = game.snapshot();
    let result = scan(&mut Cursor::new(patch), &game.root());
    assert_eq!(game.snapshot(), before, "掃描階段不得寫入");
    match result {
        Err(Fail::Reject { rule, .. }) => rule,
        other => panic!("應在掃描階段拒絕，實際 {other:?}"),
    }
}

fn rejects(patch: &[u8]) -> Rule {
    scan_rule(&Game::new(), patch)
}

/// 掃描須通過；回傳套用結果（含失敗分類）。
fn apply(game: &Game, patch: &[u8]) -> Result<(), ApplyFailure> {
    let mut reader = Cursor::new(patch);
    scan_patch(&mut reader, &game.root()).expect("scan").apply(&mut |_| {})
}

// ── builder ─────────────────────────────────────────────────────────────────

/// 合成 patch：magic 之後逐個加 chunk（CRC32 自動計算）。其他模組的測試經 `zipatch::test_support` 使用。
pub(crate) struct Patch(Vec<u8>);

impl Patch {
    pub(crate) fn new() -> Self {
        Self(MAGIC.to_vec())
    }

    fn chunk(mut self, kind: &[u8; 4], body: &[u8]) -> Self {
        let size = u32::try_from(body.len()).expect("size");
        self.0.extend_from_slice(&size.to_be_bytes());
        self.0.extend_from_slice(kind);
        self.0.extend_from_slice(body);
        let mut crc = crc32fast::Hasher::new();
        crc.update(kind);
        crc.update(body);
        self.0.extend_from_slice(&crc.finalize().to_be_bytes());
        self
    }

    pub(crate) fn sqpk(self, command: u8, sub: &[u8]) -> Self {
        let inner = i32::try_from(sub.len() + 5).expect("len");
        let body = Body::default().be_i32(inner).u8(command).bytes(sub).done();
        self.chunk(b"SQPK", &body)
    }

    /// 加上 `EOF_`（body 為 32 bytes 的 0，同樣本）並取出 bytes。
    pub(crate) fn end(self) -> Vec<u8> {
        self.chunk(b"EOF_", &[0; 32]).0
    }
}

#[derive(Default)]
struct Body(Vec<u8>);

impl Body {
    fn u8(mut self, v: u8) -> Self {
        self.0.push(v);
        self
    }

    fn be16(self, v: u16) -> Self {
        self.bytes(&v.to_be_bytes())
    }

    fn be_i16(self, v: i16) -> Self {
        self.bytes(&v.to_be_bytes())
    }

    fn be32(self, v: u32) -> Self {
        self.bytes(&v.to_be_bytes())
    }

    fn be_i32(self, v: i32) -> Self {
        self.bytes(&v.to_be_bytes())
    }

    fn be64(self, v: u64) -> Self {
        self.bytes(&v.to_be_bytes())
    }

    fn be_i64(self, v: i64) -> Self {
        self.bytes(&v.to_be_bytes())
    }

    fn le32(self, v: u32) -> Self {
        self.bytes(&v.to_le_bytes())
    }

    fn le_i32(self, v: i32) -> Self {
        self.bytes(&v.to_le_bytes())
    }

    fn le64(self, v: u64) -> Self {
        self.bytes(&v.to_le_bytes())
    }

    fn target(self, main: u16, sub: u16, file: u32) -> Self {
        self.be16(main).be16(sub).be32(file)
    }

    fn zeros(self, len: usize) -> Self {
        self.bytes(&vec![0; len])
    }

    fn bytes(mut self, v: &[u8]) -> Self {
        self.0.extend_from_slice(v);
        self
    }

    fn done(self) -> Vec<u8> {
        self.0
    }
}

fn len_u32(bytes: &[u8]) -> u32 {
    u32::try_from(bytes.len()).expect("len")
}

/// FHDR 版本 3、類型 `DIFF`，body 244 bytes（同樣本）。
fn fhdr_v3() -> Vec<u8> {
    Body::default()
        .bytes(&[0, 0, 3, 0])
        .bytes(b"DIFF")
        .be32(0)
        .zeros(244 - 12)
        .done()
}

fn aply(kind: u32, value: u32) -> Vec<u8> {
    Body::default().be32(kind).be32(4).be32(value).done()
}

fn apfs() -> Vec<u8> {
    Body::default().be64(0).be64(0).done()
}

fn dir_cmd(name: &[u8]) -> Vec<u8> {
    Body::default().be32(len_u32(name)).bytes(name).done()
}

/// SQPK/T，body 123 bytes（同樣本）。
fn target_info(platform: u16) -> Vec<u8> {
    Body::default()
        .zeros(3)
        .be16(platform)
        .be_i16(-1)
        .be_i16(0)
        .be16(0)
        .le64(0)
        .le64(0)
        .zeros(96)
        .done()
}

fn patch_info() -> Vec<u8> {
    Body::default().u8(1).u8(0).zeros(1).be64(0).done()
}

fn index_cmd(sub: u16) -> Vec<u8> {
    Body::default()
        .u8(b'A')
        .u8(0)
        .zeros(1)
        .target(1, sub, 0)
        .be64(0)
        .be32(0)
        .be32(0)
        .done()
}

/// SQPK/A：`data` 長度須為 128 的倍數。
pub(crate) fn add_data(
    main: u16,
    sub: u16,
    file: u32,
    block_offset: u32,
    data: &[u8],
    delete: u32,
) -> Vec<u8> {
    assert!(data.len().is_multiple_of(128));
    Body::default()
        .zeros(3)
        .target(main, sub, file)
        .be32(block_offset)
        .be32(len_u32(data) / 128)
        .be32(delete)
        .bytes(data)
        .done()
}

fn empty_blocks(main: u16, sub: u16, file: u32, block_offset: u32, count: u32) -> Vec<u8> {
    Body::default()
        .zeros(3)
        .target(main, sub, file)
        .be32(block_offset)
        .be32(count)
        .be32(0)
        .done()
}

fn header(file_kind: u8, header_kind: u8, main: u16, sub: u16, file: u32, data: &[u8]) -> Vec<u8> {
    Body::default()
        .u8(file_kind)
        .u8(header_kind)
        .zeros(1)
        .target(main, sub, file)
        .bytes(data)
        .done()
}

fn file_cmd(operation: u8, offset: i64, size: i64, path: &[u8], blocks: &[u8]) -> Vec<u8> {
    Body::default()
        .u8(operation)
        .zeros(2)
        .be_i64(offset)
        .be_i64(size)
        .be32(len_u32(path))
        .be16(0)
        .zeros(2)
        .bytes(path)
        .bytes(blocks)
        .done()
}

/// SQPK/F AddFile；FileSize 為各區塊 DecompressedSize 總和時傳 `size`。
fn add_file(path: &[u8], offset: i64, blocks: &[Vec<u8>], size: i64) -> Vec<u8> {
    file_cmd(b'A', offset, size, path, &blocks.concat())
}

/// 資料區塊：16 bytes 標頭＋資料，補 0 到 `(L + 143) & !0x7F`。
fn block_with(header_size: i32, compressed: i32, decompressed: i32, payload: &[u8]) -> Vec<u8> {
    let stored = if compressed == 0x7d00 { decompressed } else { compressed };
    let total = (usize::try_from(stored).expect("len") + 143) & !0x7f;
    let mut block = Body::default()
        .le_i32(header_size)
        .le32(0)
        .le_i32(compressed)
        .le_i32(decompressed)
        .bytes(payload)
        .done();
    block.resize(total.max(block.len()), 0);
    block
}

fn raw_block(data: &[u8]) -> Vec<u8> {
    block_with(16, 0x7d00, i32::try_from(data.len()).expect("len"), data)
}

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).expect("deflate");
    encoder.finish().expect("deflate")
}

fn deflate_block(data: &[u8]) -> Vec<u8> {
    let compressed = deflate(data);
    block_with(
        16,
        i32::try_from(compressed.len()).expect("len"),
        i32::try_from(data.len()).expect("len"),
        &compressed,
    )
}

/// 不可壓縮的位元組（xorshift），讓 Deflate 串流夠長以便截斷。
fn noise(len: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_u32;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state.to_le_bytes()[0]
        })
        .collect()
}

// ── 實際 patch ───────────────────────────────────────────────────────────────

#[test]
fn real_ex1_patch_scans() {
    let game = Game::new();
    let mut reader = Cursor::new(TW_EX1_PATCH);
    let scanned = scan_patch(&mut reader, &game.root()).expect("scan");
    assert_eq!(scanned.total_bytes(), 52_848);
}

#[test]
fn real_ex1_patch_applies_inside_game_root() {
    // 下列偏移取自 patch 本身：先確認 chunk 位置與子命令，避免比對到錯的區段。
    // SQPK/A chunk 在 484（子命令 body 從 497 起，資料在 +23）；SQPK/H 在 50700 與 51752
    // （子命令 body 從 +13 起，標頭資料在 +11）。
    assert_eq!(&TW_EX1_PATCH[488..492], b"SQPK");
    assert_eq!(TW_EX1_PATCH[496], b'A');
    assert_eq!(&TW_EX1_PATCH[50_704..50_708], b"SQPK");
    assert_eq!(&TW_EX1_PATCH[50_712..50_715], b"HDV");
    assert_eq!(&TW_EX1_PATCH[51_756..51_760], b"SQPK");
    assert_eq!(&TW_EX1_PATCH[51_764..51_767], b"HDD");

    let game = Game::new();
    let mut reader = Cursor::new(TW_EX1_PATCH);
    let mut progress = Vec::new();
    scan_patch(&mut reader, &game.root())
        .expect("scan")
        .apply(&mut |done| progress.push(done))
        .expect("apply");

    // 唯一寫出的檔是 SQPK A／H 的目標 dat（MainId 0x03、SubId 0x0100、FileId 0），在根目錄內。
    let dat_rel = Path::new("game/sqpack/ex1/030100.win32.dat0");
    assert_eq!(list_files(game.dir.path()), [dat_rel]);
    let dat = game.dir.path().join(dat_rel);
    assert!(dat.starts_with(game.root()));

    // SQPK/A：BlockOffset 671274、DataSize 392、DeleteSize 40（皆 << 7）。
    let a_offset = 671_274_u64 << 7;
    assert_eq!(read_at(&dat, a_offset, 392 << 7), &TW_EX1_PATCH[520..520 + (392 << 7)]);
    assert!(read_at(&dat, a_offset + (392 << 7), 40 << 7).iter().all(|&b| b == 0));
    assert_eq!(
        fs::metadata(&dat).expect("metadata").len(),
        a_offset + (392 << 7) + (40 << 7)
    );
    // SQPK/H：D＋V 寫在偏移 0，D＋D 寫在偏移 1024，各 1024 bytes。
    assert_eq!(read_at(&dat, 0, 1024), &TW_EX1_PATCH[50_724..51_748]);
    assert_eq!(read_at(&dat, 1024, 1024), &TW_EX1_PATCH[51_776..52_800]);

    // 進度：每個 chunk 結尾遞增回報，最後等於檔長。
    assert_eq!(progress.len(), 9);
    assert!(progress.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(progress.last(), Some(&52_848));
}

// ── §5.4.2 檔頭與 chunk 外框 ─────────────────────────────────────────────────

#[test]
fn rejects_bad_magic() {
    let mut first = Patch::new().end();
    first[0] = b'Z';
    let mut last = Patch::new().end();
    last[11] = 0;
    let empty: &[u8] = &[];
    for patch in [first.as_slice(), last.as_slice(), empty, &MAGIC[..5]] {
        assert_eq!(rejects(patch), Rule::Magic);
    }
}

#[test]
fn rejects_crc_mismatch() {
    let base = Patch::new().sqpk(b'X', &patch_info()).end();
    // 第一個 chunk 在 12：body 的一個 byte、CRC 本身；以及 EOF_ 的 CRC。
    for at in [12 + 8 + 5, 12 + 8 + 16, base.len() - 1] {
        let mut patch = base.clone();
        patch[at] ^= 0x01;
        assert_eq!(rejects(&patch), Rule::Crc, "byte {at}");
    }
}

#[test]
fn rejects_truncated_patch() {
    // X chunk 佔 12..40，T chunk 佔 40..180，EOF_ 佔 180..224。
    let full = Patch::new()
        .sqpk(b'X', &patch_info())
        .sqpk(b'T', &target_info(0))
        .end();
    assert_eq!(full.len(), 224);
    // 截在：外框中、body 中、CRC 中、EOF_ body 中、EOF_ CRC 中。
    for cut in [14, 40 + 8 + 50, 178, 200, 223] {
        assert_eq!(rejects(&full[..cut]), Rule::Truncated, "cut {cut}");
    }
}

#[test]
fn rejects_missing_eof() {
    assert_eq!(rejects(&Patch::new().sqpk(b'X', &patch_info()).0), Rule::MissingEof);
    assert_eq!(rejects(&MAGIC), Rule::MissingEof);
}

#[test]
fn rejects_bytes_after_eof() {
    let mut one = Patch::new().end();
    one.push(0);
    // 尾隨一個完整合法的 chunk 也拒絕。
    let mut chunk = Patch::new().end();
    chunk.extend_from_slice(&Patch::new().sqpk(b'X', &patch_info()).0[MAGIC.len()..]);
    for patch in [one, chunk] {
        assert_eq!(rejects(&patch), Rule::TrailingData);
    }
}

#[test]
fn rejects_chunk_size_over_i32_max() {
    let mut patch = Patch::new().end();
    patch[12..16].copy_from_slice(&0x8000_0000_u32.to_be_bytes());
    assert_eq!(rejects(&patch), Rule::ChunkTooLarge);
}

#[test]
fn rejects_unknown_chunk_types() {
    for kind in [b"ENTF", b"fhdr", b"EOF "] {
        assert_eq!(rejects(&Patch::new().chunk(kind, &[0; 8]).end()), Rule::UnknownChunk);
    }
}

// ── §5.4.1 長度不足、字串 ────────────────────────────────────────────────────

#[test]
fn rejects_fields_beyond_chunk_body() {
    let short_fhdr = Body::default().bytes(&[0, 0, 3, 0]).bytes(b"DIFF").be32(0).done();
    let short_dir = Body::default().be32(10).bytes(b"abc").done();
    let short_a = Body::default()
        .zeros(3)
        .target(0x0a, 0, 0)
        .be32(0)
        .be32(2)
        .be32(0)
        .zeros(128)
        .done();
    let short_f = Body::default()
        .u8(b'M')
        .zeros(2)
        .be_i64(0)
        .be_i64(0)
        .be32(100)
        .be16(0)
        .zeros(2)
        .bytes(b"x")
        .done();
    let cases = [
        ("FHDR v3", Patch::new().chunk(b"FHDR", &short_fhdr).end()),
        ("APLY", Patch::new().chunk(b"APLY", &[0; 8]).end()),
        ("APFS", Patch::new().chunk(b"APFS", &[0; 15]).end()),
        ("ADIR", Patch::new().chunk(b"ADIR", &short_dir).end()),
        ("SQPK 外框", Patch::new().chunk(b"SQPK", &[0; 3]).end()),
        ("SQPK/T", Patch::new().sqpk(b'T', &[0; 26]).end()),
        ("SQPK/X", Patch::new().sqpk(b'X', &[0; 10]).end()),
        ("SQPK/I", Patch::new().sqpk(b'I', &[0; 26]).end()),
        ("SQPK/A", Patch::new().sqpk(b'A', &short_a).end()),
        ("SQPK/D", Patch::new().sqpk(b'D', &[0; 22]).end()),
        ("SQPK/H", Patch::new().sqpk(b'H', &header(b'D', b'V', 0x0a, 0, 0, &[0; 1000])).end()),
        ("SQPK/F", Patch::new().sqpk(b'F', &short_f).end()),
    ];
    for (name, patch) in cases {
        assert_eq!(rejects(&patch), Rule::Short, "{name}");
    }
}

#[test]
fn fhdr_before_version_3_needs_only_base_fields() {
    let fhdr_v2 = Body::default().bytes(&[0, 0, 2, 0]).bytes(b"DIFF").be32(0).done();
    let game = Game::new();
    apply(&game, &Patch::new().chunk(b"FHDR", &fhdr_v2).end()).expect("apply");
}

#[test]
fn rejects_non_printable_path_bytes() {
    for path in [&b"a\x01.dll"[..], b"caf\xc3\xa9.dll", b"a\0b.dll", b"tab\t.dll", b"\x7f"] {
        let patch = Patch::new().sqpk(b'F', &file_cmd(b'M', 0, 0, path, &[])).end();
        assert_eq!(rejects(&patch), Rule::NonAscii, "{path:?}");
    }
}

// ── §5.4.3 chunk 種類 ───────────────────────────────────────────────────────

#[test]
fn header_option_and_nop_commands_write_nothing() {
    let game = Game::new();
    let before = game.snapshot();
    let patch = Patch::new()
        .chunk(b"FHDR", &fhdr_v3())
        .chunk(b"APLY", &aply(1, 0))
        .chunk(b"APLY", &aply(2, 1))
        .chunk(b"APFS", &apfs())
        .chunk(b"XXXX", &[])
        .sqpk(b'X', &patch_info())
        .sqpk(b'T', &target_info(0))
        .sqpk(b'I', &index_cmd(0x0100))
        .end();
    apply(&game, &patch).expect("apply");
    assert_eq!(game.snapshot(), before);
}

#[test]
fn adir_creates_directories_relative_to_root() {
    let game = Game::new();
    let patch = Patch::new()
        .chunk(b"ADIR", &dir_cmd(b"\\movie\\ex1\\sub"))
        .chunk(b"ADIR", &dir_cmd(b"/a/b"))
        .end();
    apply(&game, &patch).expect("apply");
    assert!(game.path("movie/ex1/sub").is_dir());
    assert!(game.path("a/b").is_dir());
}

#[test]
fn deld_removes_empty_directory_only() {
    let game = Game::new();
    fs::create_dir_all(game.path("movie/old")).expect("mkdir");
    apply(&game, &Patch::new().chunk(b"DELD", &dir_cmd(b"\\movie\\old")).end()).expect("apply");
    assert!(!game.path("movie/old").exists());
    assert!(game.path("movie").is_dir());
}

#[test]
fn deld_fails_on_missing_or_non_empty_directory() {
    let game = Game::new();
    fs::create_dir_all(game.path("movie/full")).expect("mkdir");
    fs::write(game.path("movie/full/keep.bk2"), b"keep").expect("write");
    for name in [&b"\\movie\\missing"[..], b"\\movie\\full"] {
        let err = apply(&game, &Patch::new().chunk(b"DELD", &dir_cmd(name)).end())
            .expect_err("must fail");
        assert!(matches!(err, ApplyFailure::AfterWrite(AppError::Io { .. })), "{err:?}");
    }
    assert_eq!(fs::read(game.path("movie/full/keep.bk2")).expect("read"), b"keep");
}

#[test]
fn rejects_directory_chunks_pointing_at_root() {
    for kind in [b"ADIR", b"DELD"] {
        for name in [&b""[..], b"\\", b"/", b"\\\\"] {
            let patch = Patch::new().chunk(kind, &dir_cmd(name)).end();
            assert_eq!(rejects(&patch), Rule::UnsafePath, "{name:?}");
        }
    }
}

// ── §5.4.4 SQPK ─────────────────────────────────────────────────────────────

#[test]
fn rejects_sqpk_inner_length_mismatch() {
    for inner in [99, -1, 0] {
        let body = Body::default().be_i32(inner).u8(b'X').bytes(&patch_info()).done();
        assert_eq!(rejects(&Patch::new().chunk(b"SQPK", &body).end()), Rule::SqpkLength);
    }
}

#[test]
fn rejects_unknown_sqpk_commands() {
    for command in [b'Z', b'R', b'a', 0] {
        let patch = Patch::new().sqpk(command, &[0; 64]).end();
        assert_eq!(rejects(&patch), Rule::UnknownSqpk, "{command}");
    }
}

#[test]
fn rejects_target_info_platform_other_than_win32() {
    for platform in [1, 2, 3] {
        let patch = Patch::new().sqpk(b'T', &target_info(platform)).end();
        assert_eq!(rejects(&patch), Rule::Platform);
    }
}

#[test]
fn add_data_writes_data_then_zeros_and_extends_file() {
    let game = Game::new();
    let dat = game.path("sqpack/ffxiv/0a0000.win32.dat0");
    fs::write(&dat, [0xaa_u8; 1024]).expect("seed");
    let data: Vec<u8> = (0..=255).collect();
    let patch = Patch::new()
        // 偏移 256 寫 256 bytes，再寫 384 個 0。
        .sqpk(b'A', &add_data(0x0a, 0x0000, 0, 2, &data, 3))
        // 偏移 2048（超過檔尾）寫 128 bytes，再寫 128 個 0。
        .sqpk(b'A', &add_data(0x0a, 0x0000, 0, 16, &data[..128], 1))
        .end();
    apply(&game, &patch).expect("apply");
    let bytes = fs::read(dat).expect("read");
    assert_eq!(bytes.len(), 2304);
    assert!(bytes[..256].iter().all(|&b| b == 0xaa));
    assert_eq!(&bytes[256..512], data.as_slice());
    assert!(bytes[512..896].iter().all(|&b| b == 0));
    assert!(bytes[896..1024].iter().all(|&b| b == 0xaa));
    assert!(bytes[1024..2048].iter().all(|&b| b == 0));
    assert_eq!(&bytes[2048..2176], &data[..128]);
    assert!(bytes[2176..].iter().all(|&b| b == 0));
}

#[test]
fn add_data_does_not_create_missing_sqpack_folder() {
    let game = Game::new();
    let patch = Patch::new().sqpk(b'A', &add_data(0x0a, 0x0200, 0, 0, &[1; 128], 0)).end();
    let err = apply(&game, &patch).expect_err("ex2 資料夾不存在");
    assert!(matches!(err, ApplyFailure::AfterWrite(AppError::Io { .. })), "{err:?}");
    assert!(!game.path("sqpack/ex2").exists());
}

#[test]
fn delete_and_expand_data_write_empty_blocks() {
    for command in *b"DE" {
        let game = Game::new();
        let dat = game.path("sqpack/ex1/030100.win32.dat1");
        fs::write(&dat, [0xaa_u8; 1024]).expect("seed");
        let patch = Patch::new().sqpk(command, &empty_blocks(0x03, 0x0100, 1, 1, 3)).end();
        apply(&game, &patch).expect("apply");
        let bytes = fs::read(dat).expect("read");
        let mut cleared = [0u8; 384];
        cleared[..4].copy_from_slice(&128u32.to_le_bytes());
        cleared[12..20].copy_from_slice(&2u64.to_le_bytes());
        assert_eq!(bytes.len(), 1024);
        assert!(bytes[..128].iter().all(|&b| b == 0xaa));
        assert_eq!(&bytes[128..512], cleared.as_slice());
        assert!(bytes[512..].iter().all(|&b| b == 0xaa));
    }
}

#[test]
fn rejects_zero_block_count() {
    for command in *b"DE" {
        let patch = Patch::new().sqpk(command, &empty_blocks(0x03, 0x0100, 0, 0, 0)).end();
        assert_eq!(rejects(&patch), Rule::BlockCount);
    }
}

#[test]
fn header_writes_version_header_at_zero_and_others_at_1024() {
    let game = Game::new();
    let [version, data, index, index_version] = [[1u8; 1024], [2; 1024], [3; 1024], [4; 1024]];
    let patch = Patch::new()
        .sqpk(b'H', &header(b'D', b'V', 0x03, 0x0100, 0, &version))
        .sqpk(b'H', &header(b'D', b'D', 0x03, 0x0100, 0, &data))
        .sqpk(b'H', &header(b'I', b'I', 0x03, 0x0100, 0, &index))
        .sqpk(b'H', &header(b'I', b'V', 0x03, 0x0100, 2, &index_version))
        .end();
    apply(&game, &patch).expect("apply");
    let dat = fs::read(game.path("sqpack/ex1/030100.win32.dat0")).expect("dat");
    assert_eq!(dat, [version, data].concat());
    let idx = fs::read(game.path("sqpack/ex1/030100.win32.index")).expect("index");
    assert_eq!(idx, [[0; 1024], index].concat());
    let idx2 = fs::read(game.path("sqpack/ex1/030100.win32.index2")).expect("index2");
    assert_eq!(idx2, index_version);
}

#[test]
fn rejects_unknown_header_kinds() {
    for (file_kind, header_kind) in [(b'X', b'V'), (b'D', b'X'), (b'd', b'v')] {
        let patch = Patch::new()
            .sqpk(b'H', &header(file_kind, header_kind, 0x0a, 0, 0, &[0; 1024]))
            .end();
        assert_eq!(rejects(&patch), Rule::HeaderKind);
    }
}

#[test]
fn rejects_sqpack_target_beyond_ex5() {
    for sub in [0x0600, 0xff00] {
        let cases = [
            Patch::new().sqpk(b'A', &add_data(0x0a, sub, 0, 0, &[0; 128], 0)).end(),
            Patch::new().sqpk(b'D', &empty_blocks(0x0a, sub, 0, 0, 1)).end(),
            Patch::new().sqpk(b'H', &header(b'D', b'V', 0x0a, sub, 0, &[0; 1024])).end(),
            Patch::new().sqpk(b'I', &index_cmd(sub)).end(),
        ];
        for patch in cases {
            assert_eq!(rejects(&patch), Rule::Expansion, "sub {sub:#06x}");
        }
    }
}

#[test]
fn sqpack_paths_follow_protocol() {
    let target = |main, sub, file| SqpackTarget::new(main, sub, file).expect("target");
    assert_eq!(target(0x0a, 0x0000, 0).dat(), Path::new("sqpack/ffxiv/0a0000.win32.dat0"));
    assert_eq!(target(0x03, 0x0100, 0).dat(), Path::new("sqpack/ex1/030100.win32.dat0"));
    assert_eq!(target(0x12, 0x0503, 11).dat(), Path::new("sqpack/ex5/120503.win32.dat11"));
    assert_eq!(target(0x03, 0x0100, 0).index(), Path::new("sqpack/ex1/030100.win32.index"));
    assert_eq!(target(0x03, 0x0100, 2).index(), Path::new("sqpack/ex1/030100.win32.index2"));
    assert_eq!(SqpackTarget::new(0x0a, 0x0600, 0), Err(Rule::Expansion));
}

#[test]
fn add_file_writes_raw_and_deflate_blocks_creating_directories() {
    let game = Game::new();
    let first: Vec<u8> = (0..3000_u32).map(|i| (i % 251) as u8).collect();
    let second = b"hello zipatch ".repeat(500);
    let compressed = deflate_block(&second);
    assert_ne!(&compressed[8..12], &0x7d00_i32.to_le_bytes(), "要是壓縮區塊");
    let size = i64::try_from(first.len() + second.len()).expect("size");
    // 路徑以 NUL 結尾（同樣本），混用兩種分隔符。
    let patch = Patch::new()
        .sqpk(b'F', &add_file(b"tools\\sub/new.dll\0", 0, &[raw_block(&first), compressed], size))
        .end();
    apply(&game, &patch).expect("apply");
    assert_eq!(fs::read(game.path("tools/sub/new.dll")).expect("read"), [first, second].concat());
}

#[test]
fn add_file_truncates_only_at_offset_zero() {
    let game = Game::new();
    let exe = game.path("ffxiv_dx11.exe");
    fs::write(&exe, [0xaa_u8; 4096]).expect("seed");
    // 大檔分成多個 F/A：第一段 FileOffset 0 先截斷，之後的段接著寫、不截斷。
    let patch = Patch::new()
        .sqpk(b'F', &add_file(b"ffxiv_dx11.exe", 0, &[raw_block(b"abc")], 3))
        .sqpk(b'F', &add_file(b"ffxiv_dx11.exe", 3, &[raw_block(b"def")], 3))
        .end();
    apply(&game, &patch).expect("apply");
    assert_eq!(fs::read(&exe).expect("read"), b"abcdef");
    let tail = Patch::new()
        .sqpk(b'F', &add_file(b"ffxiv_dx11.exe", 8, &[raw_block(b"xyz")], 3))
        .end();
    apply(&game, &tail).expect("apply");
    assert_eq!(fs::read(exe).expect("read"), b"abcdef\0\0xyz");
}

#[test]
fn delete_file_removes_target_and_ignores_missing() {
    let game = Game::new();
    fs::write(game.path("old.dll"), b"old").expect("seed");
    let patch = Patch::new()
        .sqpk(b'F', &file_cmd(b'D', 0, 0, b"old.dll", &[]))
        .sqpk(b'F', &file_cmd(b'D', 0, 0, b"never-existed.dll", &[]))
        .end();
    apply(&game, &patch).expect("apply");
    assert!(!game.path("old.dll").exists());
}

#[test]
fn file_written_then_deleted_in_same_patch_applies() {
    // 同一 patch 先寫後刪（拼法大小寫不同）：刪除前先關掉自己的不共用 handle，刪掉的檔不落盤、不算失敗。
    let game = Game::new();
    let patch = Patch::new()
        .sqpk(b'F', &add_file(b"tmp.dll", 0, &[raw_block(b"abc")], 3))
        .sqpk(b'F', &file_cmd(b'D', 0, 0, b"TMP.DLL", &[]))
        .end();
    apply(&game, &patch).expect("apply");
    assert!(!game.path("tmp.dll").exists());
}

#[test]
fn make_dir_tree_creates_nested_directories() {
    let game = Game::new();
    let patch = Patch::new().sqpk(b'F', &file_cmd(b'M', 0, 0, b"movie/ex9/a", &[])).end();
    apply(&game, &patch).expect("apply");
    assert!(game.path("movie/ex9/a").is_dir());
}

#[test]
fn rejects_remove_all() {
    let patch = Patch::new().sqpk(b'F', &file_cmd(b'R', 0, 0, b"", &[])).end();
    assert_eq!(rejects(&patch), Rule::RemoveAll);
}

#[test]
fn rejects_unknown_file_operations() {
    for operation in [b'X', b'a', 0] {
        let patch = Patch::new().sqpk(b'F', &file_cmd(operation, 0, 0, b"x.dll", &[])).end();
        assert_eq!(rejects(&patch), Rule::FileOperation, "{operation}");
    }
}

#[test]
fn rejects_negative_file_offset() {
    let patch = Patch::new().sqpk(b'F', &add_file(b"x.dll", -1, &[raw_block(b"abc")], 3)).end();
    assert_eq!(rejects(&patch), Rule::FileOffset);
}

#[test]
fn rejects_file_size_mismatch() {
    for size in [2, 4, -3] {
        let patch =
            Patch::new().sqpk(b'F', &add_file(b"x.dll", 0, &[raw_block(b"abc")], size)).end();
        assert_eq!(rejects(&patch), Rule::FileSize, "{size}");
    }
}

#[test]
fn rejects_block_header_size_other_than_16() {
    for header_size in [0, 15, 32] {
        let block = block_with(header_size, 0x7d00, 3, b"abc");
        let patch = Patch::new().sqpk(b'F', &add_file(b"x.dll", 0, &[block], 3)).end();
        assert_eq!(rejects(&patch), Rule::BlockHeader, "{header_size}");
    }
}

#[test]
fn rejects_negative_block_sizes() {
    let negative_compressed = Body::default().le_i32(16).le32(0).le_i32(-1).le_i32(3).zeros(112).done();
    let negative_raw = Body::default().le_i32(16).le32(0).le_i32(0x7d00).le_i32(-1).zeros(112).done();
    for block in [negative_compressed, negative_raw] {
        let patch = Patch::new().sqpk(b'F', &add_file(b"x.dll", 0, &[block], 3)).end();
        assert_eq!(rejects(&patch), Rule::BlockSize);
    }
}

#[test]
fn rejects_blocks_not_ending_at_body_end() {
    // 最後一個區塊之後多出不足一個標頭的 bytes。
    let mut extra = raw_block(b"abc");
    extra.extend_from_slice(&[0; 10]);
    // 標頭宣告的區塊總長超出子命令 body。
    let mut cut = raw_block(&[7; 300]);
    cut.truncate(200);
    for blocks in [extra, cut] {
        let patch = Patch::new().sqpk(b'F', &add_file(b"x.dll", 0, &[blocks], 3)).end();
        assert_eq!(rejects(&patch), Rule::BlockBounds);
    }
}

#[test]
fn rejects_corrupt_deflate() {
    // 0xff：BFINAL＝1、BTYPE＝11（保留值）。
    let garbage = block_with(16, 64, 100, &[0xff; 64]);
    // 串流截在 240 bytes：區塊總長 256，資料範圍正好 240 bytes、沒有填充，輸入用完時串流仍未結束。
    let stream = deflate(&noise(2000));
    assert!(stream.len() > 240);
    let truncated = block_with(16, 240, 2000, &stream[..240]);
    for (block, size) in [(garbage, 100), (truncated, 2000)] {
        let patch = Patch::new().sqpk(b'F', &add_file(b"x.dll", 0, &[block], size)).end();
        assert_eq!(rejects(&patch), Rule::Deflate);
    }
}

#[test]
fn rejects_decompressed_length_mismatch() {
    let data = b"zipatch ".repeat(100);
    let compressed = deflate(&data);
    let compressed_len = i32::try_from(compressed.len()).expect("len");
    for claimed in [799, 801] {
        let block = block_with(16, compressed_len, claimed, &compressed);
        let patch =
            Patch::new().sqpk(b'F', &add_file(b"x.dll", 0, &[block], i64::from(claimed))).end();
        assert_eq!(rejects(&patch), Rule::DecompressedLength, "{claimed}");
    }
}

// ── 套用中的共用（主 note「遊戲執行判定」） ─────────────────────────────────────

const DAT0: &str = "sqpack/ex1/030100.win32.dat0";
const DAT1: &str = "sqpack/ex1/030100.win32.dat1";

/// 兩個寫入動作：SQPK/A 先寫 ex1 的 `.dat0`，再寫 `.dat1`。
fn two_dat_patch() -> Vec<u8> {
    Patch::new()
        .sqpk(b'A', &add_data(0x03, 0x0100, 0, 0, &[0x11; 128], 0))
        .sqpk(b'A', &add_data(0x03, 0x0100, 1, 0, &[0x22; 128], 0))
        .end()
}

/// 以讀取共用開著（同遊戲讀 sqpack 的方式），不允許其他 handle 寫入。
fn hold_for_read(path: &Path) -> fs::File {
    fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(path)
        .expect("hold")
}

#[test]
fn file_held_open_before_first_write_is_before_write() {
    let game = Game::new();
    fs::write(game.path(DAT0), b"").expect("seed");
    let before = game.snapshot();
    let holder = hold_for_read(&game.path(DAT0));
    let result = apply(&game, &two_dat_patch());
    drop(holder);
    assert!(
        matches!(result, Err(ApplyFailure::BeforeWrite(AppError::GameRunning))),
        "{result:?}"
    );
    assert_eq!(game.snapshot(), before, "第一筆寫入前中止，遊戲目錄不變");
}

#[test]
fn file_held_open_after_directory_change_is_after_write() {
    // 共用違規前已建過目錄：遊戲目錄已改動，依已寫入處理。
    let game = Game::new();
    fs::write(game.path(DAT0), b"").expect("seed");
    let holder = hold_for_read(&game.path(DAT0));
    let patch = Patch::new()
        .chunk(b"ADIR", &dir_cmd(b"\\movie\\new"))
        .sqpk(b'A', &add_data(0x03, 0x0100, 0, 0, &[0x11; 128], 0))
        .end();
    let result = apply(&game, &patch);
    drop(holder);
    assert!(matches!(result, Err(ApplyFailure::AfterWrite(AppError::Io { .. }))), "{result:?}");
    assert!(game.path("movie/new").is_dir());
}

#[test]
fn file_opened_by_another_handle_mid_apply_is_after_write() {
    let game = Game::new();
    fs::write(game.path(DAT1), b"").expect("seed");
    let patch = two_dat_patch();
    let mut reader = Cursor::new(patch.as_slice());
    let mut holder = None;
    let result = scan_patch(&mut reader, &game.root()).expect("scan").apply(&mut |_| {
        // 第一個動作（寫 .dat0）之後、第二個動作開 .dat1 之前，另一個 handle 以讀取共用開著 .dat1。
        if holder.is_none() {
            holder = Some(hold_for_read(&game.path(DAT1)));
        }
    });
    drop(holder);
    assert!(matches!(result, Err(ApplyFailure::AfterWrite(AppError::Io { .. }))), "{result:?}");
    assert_eq!(fs::read(game.path(DAT0)).expect("dat0"), [0x11; 128]);
    assert_eq!(fs::read(game.path(DAT1)).expect("dat1"), b"");
}

#[test]
fn written_file_is_locked_until_synced() {
    // 套用中已寫過的檔（含本 patch 新建的）：其他 handle 讀不了、寫不了、刪不掉、改不了名，直到落盤後才放開。
    let game = Game::new();
    let dll = game.path("new.dll");
    let patch = Patch::new()
        .sqpk(b'F', &add_file(b"new.dll", 0, &[raw_block(b"abc")], 3))
        .sqpk(b'A', &add_data(0x03, 0x0100, 0, 0, &[0x11; 128], 0))
        .end();
    let mut reader = Cursor::new(patch.as_slice());
    let mut attempts = Vec::new();
    scan_patch(&mut reader, &game.root())
        .expect("scan")
        .apply(&mut |_| {
            attempts.push([
                fs::File::open(&dll).is_err(),
                fs::OpenOptions::new().write(true).open(&dll).is_err(),
                fs::remove_file(&dll).is_err(),
                fs::rename(&dll, game.path("moved.dll")).is_err(),
            ]);
        })
        .expect("apply");
    // 每個動作之後（含最後的 EOF_）都還在套用中。
    assert_eq!(attempts, [[true; 4]; 3]);
    assert_eq!(fs::read(&dll).expect("read"), b"abc");
    assert!(!game.path("moved.dll").exists());
}

// ── §5.4.5 輸出路徑 ─────────────────────────────────────────────────────────

/// 同一個路徑放進所有帶路徑的動作：ADIR、DELD、SQPK/F 的 A、D、M。
fn assert_unsafe_path(path: &str) {
    let raw = path.as_bytes();
    let sources = [
        Patch::new().chunk(b"ADIR", &dir_cmd(raw)).end(),
        Patch::new().chunk(b"DELD", &dir_cmd(raw)).end(),
        Patch::new().sqpk(b'F', &add_file(raw, 0, &[raw_block(b"x")], 1)).end(),
        Patch::new().sqpk(b'F', &file_cmd(b'D', 0, 0, raw, &[])).end(),
        Patch::new().sqpk(b'F', &file_cmd(b'M', 0, 0, raw, &[])).end(),
    ];
    for patch in sources {
        assert_eq!(rejects(&patch), Rule::UnsafePath, "{path}");
    }
}

#[test]
fn rejects_parent_directory_components() {
    for path in ["..\\evil.dll", "a/../../evil.dll", "sqpack\\..\\..\\evil", "..", "\\..\\x"] {
        assert_unsafe_path(path);
    }
}

#[test]
fn rejects_absolute_paths() {
    for path in ["C:\\Windows\\evil.dll", "c:/evil.dll", "\\\\?\\C:\\evil.dll", "\\\\.\\C:\\evil"] {
        assert_unsafe_path(path);
    }
}

#[test]
fn rejects_drive_relative_path() {
    for path in ["C:evil.dll", "D:"] {
        assert_unsafe_path(path);
    }
}

#[test]
fn rejects_alternate_data_streams() {
    for path in ["ffxiv_dx11.exe:evil", "ffxiv_dx11.exe::$DATA", "sqpack:hidden\\x"] {
        assert_unsafe_path(path);
    }
}

#[test]
fn rejects_empty_dot_and_trailing_dot_components() {
    for path in ["a\\\\b", "a/", ".\\a", "a/./b", "a\\b.", "a\\.. \\b", "x\\...", "a\\b "] {
        assert_unsafe_path(path);
    }
}

#[test]
fn rejects_dos_device_names() {
    for path in ["CON", "nul", "Aux.dll", "sqpack\\PRN\\x", "com1.txt", "LPT9", "con .log", "a/Com5.x.y"] {
        assert_unsafe_path(path);
    }
    // 只是前綴相同、不是裝置名的成分照常接受。
    for path in ["console.dll", "com10", "lpt0", "nulls\\x", "COM1x.dat", "a.con"] {
        assert!(relative_path(path).is_some(), "{path}");
    }
}

#[test]
fn leading_separators_make_paths_relative_to_root() {
    assert_eq!(relative_path("\\movie\\ex1"), Some(PathBuf::from("movie/ex1")));
    assert_eq!(relative_path("//sqpack/ffxiv"), Some(PathBuf::from("sqpack/ffxiv")));
    assert_eq!(relative_path("\\\\server\\share\\x"), Some(PathBuf::from("server/share/x")));
    assert_eq!(relative_path("ffxiv_dx11.exe"), Some(PathBuf::from("ffxiv_dx11.exe")));
}

#[test]
fn rejects_junction_in_sqpack_path() {
    let game = Game::new();
    let outside = game.outside();
    fs::remove_dir(game.path("sqpack/ex1")).expect("rmdir");
    junction(&game.path("sqpack/ex1"), &outside);
    let patch = Patch::new().sqpk(b'A', &add_data(0x03, 0x0100, 0, 0, &[1; 128], 0)).end();
    assert_eq!(scan_rule(&game, &patch), Rule::Link);
    assert_eq!(fs::read_dir(outside).expect("read_dir").count(), 0);
}

#[test]
fn rejects_junction_in_file_path() {
    let game = Game::new();
    let outside = game.outside();
    junction(&game.path("mods"), &outside);
    let patch = Patch::new().sqpk(b'F', &add_file(b"mods\\x.dll", 0, &[raw_block(b"x")], 1)).end();
    assert_eq!(scan_rule(&game, &patch), Rule::Link);
    assert_eq!(fs::read_dir(outside).expect("read_dir").count(), 0);
}

#[test]
fn rejects_junction_as_game_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let real = dir.path().join("real");
    fs::create_dir(&real).expect("mkdir");
    let root = dir.path().join("game");
    junction(&root, &real);
    let patch = Patch::new().chunk(b"ADIR", &dir_cmd(b"\\x")).end();
    let result = scan(&mut Cursor::new(patch.as_slice()), &root);
    assert!(matches!(result, Err(Fail::Reject { rule: Rule::Link, .. })), "{result:?}");
    assert_eq!(fs::read_dir(real).expect("read_dir").count(), 0);
}

#[test]
fn apply_rechecks_links_before_writing() {
    let game = Game::new();
    let outside = game.outside();
    let patch = Patch::new().sqpk(b'A', &add_data(0x03, 0x0100, 0, 0, &[1; 128], 0)).end();
    let mut reader = Cursor::new(patch.as_slice());
    let scanned = scan_patch(&mut reader, &game.root()).expect("scan");
    // 掃描通過後才把 sqpack\ex1 換成 junction。
    fs::remove_dir(game.path("sqpack/ex1")).expect("rmdir");
    junction(&game.path("sqpack/ex1"), &outside);
    let err = scanned.apply(&mut |_| {}).expect_err("must reject");
    assert!(matches!(err, ApplyFailure::AfterWrite(AppError::PatchRejected { .. })), "{err:?}");
    assert_eq!(fs::read_dir(outside).expect("read_dir").count(), 0);
}

#[test]
fn scan_failure_is_patch_rejected_without_patch_content() {
    let game = Game::new();
    let patch = Patch::new()
        .sqpk(b'F', &file_cmd(b'M', 0, 0, b"..\\secret-name", &[]))
        .end();
    let mut reader = Cursor::new(patch.as_slice());
    let Err(err) = scan_patch(&mut reader, &game.root()) else {
        panic!("應拒絕");
    };
    let message = err.to_string();
    assert!(matches!(err, AppError::PatchRejected { .. }), "{message}");
    assert!(message.contains("偏移 12"), "{message}");
    assert!(!message.contains("secret-name"), "{message}");
}

// ── .ver ────────────────────────────────────────────────────────────────────

#[test]
fn write_version_targets_protocol_paths_and_replaces_old_value() {
    let game = Game::new();
    write_version(&game.root(), Repo::Game, "2026.09.14.0000.0000").expect("game");
    write_version(&game.root(), Repo::Expansion(1), "2026.08.07.0000.0000").expect("ex1");
    assert_eq!(
        fs::read_to_string(game.path("ffxivgame.ver")).expect("read"),
        "2026.09.14.0000.0000"
    );
    assert_eq!(
        fs::read_to_string(game.path("sqpack/ex1/ex1.ver")).expect("read"),
        "2026.08.07.0000.0000"
    );
    // game.rs 讀版本的路徑與這裡寫入的一致（安裝根目錄＝`game` 的上層）。
    let install = game.dir.path();
    assert_eq!(game::game_version(install).expect("read"), "2026.09.14.0000.0000");
    assert_eq!(
        game::expansion_version(install, 1).expect("read").as_deref(),
        Some("2026.08.07.0000.0000")
    );

    // 覆寫：舊值較長且帶換行，確認不殘留。
    fs::write(game.path("sqpack/ex1/ex1.ver"), "2026.07.03.0000.0000\r\n").expect("seed");
    write_version(&game.root(), Repo::Expansion(1), "2026.08.10.0000.0000").expect("overwrite");
    assert_eq!(
        fs::read_to_string(game.path("sqpack/ex1/ex1.ver")).expect("read"),
        "2026.08.10.0000.0000"
    );
    // 沒有殘留暫存檔，也沒寫到其他地方。
    assert_eq!(
        list_files(install),
        [Path::new("game/ffxivgame.ver"), Path::new("game/sqpack/ex1/ex1.ver")]
    );
}

#[test]
fn write_version_rejects_repo_outside_game_and_ex1_to_ex5() {
    let game = Game::new();
    for n in [0, 6] {
        let err = write_version(&game.root(), Repo::Expansion(n), "2026.08.07.0000.0000")
            .expect_err("must reject");
        assert!(matches!(err, AppError::PatchRejected { .. }), "{err}");
    }
    assert!(list_files(game.dir.path()).is_empty());
}

#[test]
fn write_version_rejects_junction() {
    let game = Game::new();
    let outside = game.outside();
    fs::remove_dir(game.path("sqpack/ex1")).expect("rmdir");
    junction(&game.path("sqpack/ex1"), &outside);
    let err = write_version(&game.root(), Repo::Expansion(1), "2026.08.07.0000.0000")
        .expect_err("must reject");
    assert!(matches!(err, AppError::PatchRejected { .. }), "{err}");
    assert_eq!(fs::read_dir(outside).expect("read_dir").count(), 0);
}
