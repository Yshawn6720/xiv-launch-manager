//! chunk body 的解析（protocols §5.4.3–§5.4.4）：把一個已驗過外框與 CRC32 的 chunk 轉成套用
//! 動作 [`Op`]。只依內容、不碰檔案系統；輸出路徑在這裡完成正規化（§5.4.5）。

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use flate2::{Decompress, FlushDecompress, Status};

use super::target::{self, SqpackTarget};
use super::{Fail, Rule};

/// SQPK/F 資料區塊標頭的 CompressedSize 為此值＝未壓縮。
const UNCOMPRESSED: i32 = 0x7d00;
/// 資料區塊標頭長度；區塊總長公式 `(L + 143) & !0x7F` 隱含此值。
const BLOCK_HEADER_LEN: usize = 16;
/// SQPK/H 的標頭資料長度，也是標頭種類 `V` 以外的寫入偏移。
const SQPK_HEADER_LEN: usize = 1024;
/// FHDR 版本 3 在 EntryFiles 之後的 12 個 u32 BE。
const FHDR_V3_FIELDS_LEN: usize = 12 * 4;
const INFLATE_BUF_LEN: usize = 32 * 1024;

/// 一個 chunk 的套用動作。路徑都相對套用根目錄，且已通過 §5.4.5 的正規化。
pub(super) enum Op<'a> {
    /// 不套用任何動作：FHDR、APLY、APFS、XXXX，SQPK/T、X、I。
    Skip,
    /// `EOF_`。
    End,
    /// 建立目錄（含上層）：ADIR、SQPK/F MakeDirTree。
    MakeDir(PathBuf),
    /// 非遞迴刪除目錄（不存在或非空即失敗）：DELD。
    RemoveDir(PathBuf),
    /// 開啟 SqPack 檔（上層目錄須存在），在 `offset` 寫入 `data`，再接著寫 `zeros` 個 0：
    /// SQPK/A、H。
    Write {
        file: PathBuf,
        offset: u64,
        data: &'a [u8],
        zeros: u64,
    },
    /// 開啟 SqPack 檔（同上），從 `offset` 起清空 `count` 個 128-byte 區塊並寫空區塊標頭：
    /// SQPK/D、E。
    EmptyBlocks {
        file: PathBuf,
        offset: u64,
        count: NonZeroU32,
    },
    /// SQPK/F AddFile：建立上層目錄，`offset` 為 0 時先截斷，再從 `offset` 依序寫入各區塊
    /// 解出的資料。
    AddFile {
        file: PathBuf,
        offset: u64,
        blocks: Vec<Block<'a>>,
    },
    /// SQPK/F DeleteFile：不存在視為成功。
    DeleteFile(PathBuf),
}

impl Op<'_> {
    /// 會寫入或刪除的路徑（相對套用根目錄）。
    pub(super) fn target(&self) -> Option<&Path> {
        match self {
            Op::Skip | Op::End => None,
            Op::MakeDir(path) | Op::RemoveDir(path) | Op::DeleteFile(path) => Some(path),
            Op::Write { file, .. } | Op::EmptyBlocks { file, .. } | Op::AddFile { file, .. } => {
                Some(file)
            }
        }
    }
}

/// SQPK/F 的一個資料區塊；外框與長度已核對，Deflate 內容由 [`Block::decode`] 驗證。
pub(super) struct Block<'a> {
    /// 壓縮區塊：標頭之後到區塊尾（含填充）；未壓縮區塊：DecompressedSize bytes 原始資料。
    data: &'a [u8],
    compressed: bool,
    /// DecompressedSize。
    size: u64,
}

impl Block<'_> {
    /// 依序把解出的資料交給 `sink`。壓縮區塊為 raw Deflate（RFC 1951），串流必須在資料範圍內
    /// 結束，解出長度必須等於 DecompressedSize；之後的填充忽略。
    pub(super) fn decode(
        &self,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), Fail>,
    ) -> Result<(), Fail> {
        if !self.compressed {
            return sink(self.data);
        }
        let mut inflater = Decompress::new(false);
        let mut buf = [0u8; INFLATE_BUF_LEN];
        let mut produced = 0u64;
        loop {
            let consumed = inflater.total_in();
            let before = inflater.total_out();
            let input = self
                .data
                .get(consumed as usize..)
                .ok_or(Fail::reject(Rule::Deflate))?;
            let status = inflater
                .decompress(input, &mut buf, FlushDecompress::None)
                .map_err(|_| Fail::reject(Rule::Deflate))?;
            let out = (inflater.total_out() - before) as usize;
            produced += out as u64;
            if produced > self.size {
                return Err(Fail::reject(Rule::DecompressedLength));
            }
            sink(&buf[..out])?;
            match status {
                Status::StreamEnd => break,
                // 沒有任何進展＝輸入已用完，串流卻還沒結束。
                Status::Ok | Status::BufError => {
                    if out == 0 && inflater.total_in() == consumed {
                        return Err(Fail::reject(Rule::Deflate));
                    }
                }
            }
        }
        if produced == self.size {
            Ok(())
        } else {
            Err(Fail::reject(Rule::DecompressedLength))
        }
    }
}

/// SQPK 以 128 bytes 為單位的欄位換成 bytes（`<< 7`，64-bit 計算）。
pub(super) fn units(n: u32) -> u64 {
    u64::from(n) << 7
}

/// 解析一個 chunk 的 body（`kind` 為 4 字元 type）。
pub(super) fn parse(kind: [u8; 4], body: &[u8]) -> Result<Op<'_>, Rule> {
    let mut f = Fields::new(body);
    match &kind {
        b"FHDR" => file_header(f),
        b"APLY" => {
            f.u32_be()?; // kind
            f.skip(4)?;
            f.u32_be()?; // value：XL 7.0.20 的套用流程不讀，本專案忽略
            Ok(Op::Skip)
        }
        b"APFS" => {
            f.i64_be()?;
            f.i64_be()?;
            Ok(Op::Skip)
        }
        b"ADIR" => dir_path(f).map(Op::MakeDir),
        b"DELD" => dir_path(f).map(Op::RemoveDir),
        b"SQPK" => sqpk(body),
        b"EOF_" => Ok(Op::End),
        b"XXXX" => Ok(Op::Skip),
        _ => Err(Rule::UnknownChunk),
    }
}

/// FHDR：只讀、不驗欄位值；版本取 `+0` u32 LE 的 bits 16–23（即 `+2` 那個 byte）。
fn file_header(mut f: Fields<'_>) -> Result<Op<'_>, Rule> {
    let [_, _, version, _] = f.array::<4>()?;
    f.skip(4)?; // patch 類型
    f.u32_be()?; // EntryFiles
    if version == 3 {
        f.skip(FHDR_V3_FIELDS_LEN)?;
    }
    Ok(Op::Skip)
}

/// ADIR／DELD：u32 BE 長度＋目錄名。
fn dir_path(mut f: Fields<'_>) -> Result<PathBuf, Rule> {
    let len = f.u32_be()?;
    output_path(f.bytes(len as usize)?)
}

fn sqpk(body: &[u8]) -> Result<Op<'_>, Rule> {
    let mut f = Fields::new(body);
    let inner = f.i32_be()?;
    if usize::try_from(inner).ok() != Some(body.len()) {
        return Err(Rule::SqpkLength);
    }
    let command = f.u8()?;
    let sub = Fields::new(f.rest());
    match command {
        b'T' => target_info(sub),
        b'X' => patch_info(sub),
        b'I' => index(sub),
        b'A' => add_data(sub),
        b'D' | b'E' => empty_blocks(sub),
        b'H' => header(sub),
        b'F' => sqpk_file(sub),
        _ => Err(Rule::UnknownSqpk),
    }
}

/// SQPK/T：platform 必須是 0（Win32），其他欄位只讀不驗。
fn target_info(mut f: Fields<'_>) -> Result<Op<'_>, Rule> {
    f.skip(3)?;
    let platform = f.u16_be()?;
    f.i16_be()?; // region
    f.i16_be()?; // isDebug
    f.u16_be()?; // version
    f.u64_le()?; // DeletedDataSize
    f.u64_le()?; // SeekCount
    if platform == 0 {
        Ok(Op::Skip)
    } else {
        Err(Rule::Platform)
    }
}

/// SQPK/X：不套用。
fn patch_info(mut f: Fields<'_>) -> Result<Op<'_>, Rule> {
    f.u8()?; // status
    f.u8()?; // version
    f.skip(1)?;
    f.u64_be()?; // InstallSize
    Ok(Op::Skip)
}

/// SQPK/I：不套用；target 仍須是可識別的 SqPack 目標。
fn index(mut f: Fields<'_>) -> Result<Op<'_>, Rule> {
    f.u8()?; // A／D
    f.u8()?; // IsSynonym
    f.skip(1)?;
    sqpack_target(&mut f)?;
    f.u64_be()?; // FileHash
    f.u32_be()?; // BlockOffset
    f.u32_be()?; // BlockNumber
    Ok(Op::Skip)
}

/// SQPK/A：在 `BlockOffset << 7` 寫入 `DataSize << 7` bytes，再寫 `DeleteSize << 7` 個 0。
fn add_data(mut f: Fields<'_>) -> Result<Op<'_>, Rule> {
    f.skip(3)?;
    let target = sqpack_target(&mut f)?;
    let offset = units(f.u32_be()?);
    let data_len = units(f.u32_be()?);
    let zeros = units(f.u32_be()?);
    let data = f.bytes(usize::try_from(data_len).map_err(|_| Rule::Short)?)?;
    Ok(Op::Write {
        file: target.dat(),
        offset,
        data,
        zeros,
    })
}

/// SQPK/D、E：BlockCount 是區塊數，為 0 即失敗（XL 會寫入 −1）。
fn empty_blocks(mut f: Fields<'_>) -> Result<Op<'_>, Rule> {
    f.skip(3)?;
    let target = sqpack_target(&mut f)?;
    let offset = units(f.u32_be()?);
    let count = f.u32_be()?;
    f.u32_be()?; // 保留
    let count = NonZeroU32::new(count).ok_or(Rule::BlockCount)?;
    Ok(Op::EmptyBlocks {
        file: target.dat(),
        offset,
        count,
    })
}

/// SQPK/H：檔案種類 `D`／`I` 決定 dat 或 index；標頭種類 `V` 寫在 0，`I`／`D` 寫在 1024。
fn header(mut f: Fields<'_>) -> Result<Op<'_>, Rule> {
    let file_kind = f.u8()?;
    let header_kind = f.u8()?;
    f.skip(1)?;
    let target = sqpack_target(&mut f)?;
    let data = f.bytes(SQPK_HEADER_LEN)?;
    let file = match file_kind {
        b'D' => target.dat(),
        b'I' => target.index(),
        _ => return Err(Rule::HeaderKind),
    };
    let offset = match header_kind {
        b'V' => 0,
        b'I' | b'D' => SQPK_HEADER_LEN as u64,
        _ => return Err(Rule::HeaderKind),
    };
    Ok(Op::Write {
        file,
        offset,
        data,
        zeros: 0,
    })
}

/// SQPK/F：只接受 AddFile、DeleteFile、MakeDirTree；路徑三者都要通過 §5.4.5。
fn sqpk_file(mut f: Fields<'_>) -> Result<Op<'_>, Rule> {
    let operation = f.u8()?;
    f.skip(2)?;
    let offset = f.i64_be()?;
    let size = f.i64_be()?;
    let path_len = f.u32_be()?;
    f.u16_be()?; // ExpansionId：只有 RemoveAll 使用
    f.skip(2)?;
    let raw_path = f.bytes(path_len as usize)?;
    match operation {
        b'A' => {
            let file = output_path(raw_path)?;
            let offset = u64::try_from(offset).map_err(|_| Rule::FileOffset)?;
            let blocks = data_blocks(f.rest())?;
            let total: u64 = blocks.iter().map(|block| block.size).sum();
            if u64::try_from(size).ok() != Some(total) {
                return Err(Rule::FileSize);
            }
            Ok(Op::AddFile {
                file,
                offset,
                blocks,
            })
        }
        b'D' => output_path(raw_path).map(Op::DeleteFile),
        b'M' => output_path(raw_path).map(Op::MakeDir),
        b'R' => Err(Rule::RemoveAll),
        _ => Err(Rule::FileOperation),
    }
}

/// SQPK/F AddFile 的資料區塊，一個接一個直到子命令 body 結尾。
fn data_blocks(mut rest: &[u8]) -> Result<Vec<Block<'_>>, Rule> {
    let mut blocks = Vec::new();
    while !rest.is_empty() {
        if rest.len() < BLOCK_HEADER_LEN {
            return Err(Rule::BlockBounds);
        }
        let mut f = Fields::new(rest);
        let header_size = f.i32_le()?;
        f.u32_le()?; // 保留
        let compressed_size = f.i32_le()?;
        let decompressed_size = f.i32_le()?;
        if usize::try_from(header_size).ok() != Some(BLOCK_HEADER_LEN) {
            return Err(Rule::BlockHeader);
        }
        let (Ok(compressed_len), Ok(size)) = (
            usize::try_from(compressed_size),
            usize::try_from(decompressed_size),
        ) else {
            return Err(Rule::BlockSize);
        };
        let compressed = compressed_size != UNCOMPRESSED;
        let stored = if compressed { compressed_len } else { size };
        // 區塊總長＝「16 bytes 標頭＋資料」向上對齊到 128 bytes。
        let total = (stored + 143) & !0x7f;
        let block = rest.get(..total).ok_or(Rule::BlockBounds)?;
        let data_end = if compressed {
            total
        } else {
            BLOCK_HEADER_LEN + size
        };
        let data = block
            .get(BLOCK_HEADER_LEN..data_end)
            .ok_or(Rule::BlockBounds)?;
        blocks.push(Block {
            data,
            compressed,
            size: size as u64,
        });
        rest = &rest[total..];
    }
    Ok(blocks)
}

/// 8 bytes SqPack target：u16 BE MainId、u16 BE SubId、u32 BE FileId。
fn sqpack_target(f: &mut Fields<'_>) -> Result<SqpackTarget, Rule> {
    let main = f.u16_be()?;
    let sub = f.u16_be()?;
    let file = f.u32_be()?;
    SqpackTarget::new(main, sub, file)
}

/// 固定長度字串：去掉尾端 NUL 後只允許 0x20–0x7E（§5.4.1），再依 §5.4.5 正規化成相對路徑。
fn output_path(raw: &[u8]) -> Result<PathBuf, Rule> {
    let len = raw.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let text = &raw[..len];
    if !text.iter().all(|b| (0x20..=0x7e).contains(b)) {
        return Err(Rule::NonAscii);
    }
    let text = std::str::from_utf8(text).map_err(|_| Rule::NonAscii)?;
    target::relative_path(text).ok_or(Rule::UnsafePath)
}

/// 依序讀固定欄位；欄位超出 body 即 [`Rule::Short`]。
struct Fields<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Fields<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8], Rule> {
        let end = self.pos.checked_add(len).ok_or(Rule::Short)?;
        let out = self.buf.get(self.pos..end).ok_or(Rule::Short)?;
        self.pos = end;
        Ok(out)
    }

    fn skip(&mut self, len: usize) -> Result<(), Rule> {
        self.bytes(len)?;
        Ok(())
    }

    /// 尚未讀取的部分。
    fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Rule> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.bytes(N)?);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, Rule> {
        let [b] = self.array::<1>()?;
        Ok(b)
    }

    fn u16_be(&mut self) -> Result<u16, Rule> {
        self.array().map(u16::from_be_bytes)
    }

    fn i16_be(&mut self) -> Result<i16, Rule> {
        self.array().map(i16::from_be_bytes)
    }

    fn u32_be(&mut self) -> Result<u32, Rule> {
        self.array().map(u32::from_be_bytes)
    }

    fn i32_be(&mut self) -> Result<i32, Rule> {
        self.array().map(i32::from_be_bytes)
    }

    fn u32_le(&mut self) -> Result<u32, Rule> {
        self.array().map(u32::from_le_bytes)
    }

    fn i32_le(&mut self) -> Result<i32, Rule> {
        self.array().map(i32::from_le_bytes)
    }

    fn u64_be(&mut self) -> Result<u64, Rule> {
        self.array().map(u64::from_be_bytes)
    }

    fn u64_le(&mut self) -> Result<u64, Rule> {
        self.array().map(u64::from_le_bytes)
    }

    fn i64_be(&mut self) -> Result<i64, Rule> {
        self.array().map(i64::from_be_bytes)
    }
}
