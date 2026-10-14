//! Patch list 解析（protocols §5.1）與 patch 檔區塊 SHA1 核對（§5.2）。
//!
//! 失敗關閉：任何一行不合規格就整批拒絕，不回傳部分結果。錯誤原因只帶規則與行號，
//! 不回顯清單內容或 URL（國際服清單來自含 SID 的 session 註冊請求）。

use std::fmt;
use std::io::{self, Read};

use reqwest::Url;
use sha1::{Digest, Sha1};

use crate::error::{AppError, AppResult};
use crate::region::Region;

/// `hashBlockSize` 的上下限（本專案訂定，非上游規格；protocols §5.1）。
const MIN_BLOCK_SIZE: u64 = 1_048_576;
const MAX_BLOCK_SIZE: u64 = 268_435_456;
/// multipart boundary 的長度上限（RFC 2046）。
const MAX_BOUNDARY_LEN: usize = 70;
const PATCH_SUFFIX: &str = ".patch";
const READ_BUF_SIZE: usize = 64 * 1024;

/// patch 所屬的 repo（protocols §5.1 路徑表）。套用根目錄一律 `{install}\game`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Repo {
    /// 基本遊戲（ffxiv），`.ver` 為 `game\ffxivgame.ver`。
    Game,
    /// `exN`（N＝1–5），`.ver` 為 `game\sqpack\exN\exN.ver`。
    Expansion(u8),
}

/// 清單中的一筆 patch，欄位都已通過 §5.1 的規則。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchEntry {
    /// `[0]`，patch 檔長度（bytes）。
    pub length: u64,
    /// `[4]`，套用成功後原樣寫入 `.ver`。
    pub version_id: String,
    /// `[6]`。
    pub hash_block_size: u64,
    /// `[7]`，每段的 SHA1（小寫 hex），個數＝`ceil(length / hash_block_size)`。
    pub hashes: Vec<String>,
    /// `[8]` 的實際下載位址：host 已確認為該區服的 patch host；台服一律改成 `https`（[`download_url`]）。
    pub url: Url,
    pub repo: Repo,
    /// URL 最後一段，已確認是單一安全路徑成分。
    pub file_name: String,
}

/// 解析 patch list。只在呼叫端已判定「需要更新」時呼叫（國際服 2xx＋UID＋非空 body；
/// 台服 2xx 且含 patch URL），因此解析不出任何 patch 行也是錯誤。
pub fn parse_patch_list(
    region: Region,
    content_type: &str,
    body: &[u8],
) -> AppResult<Vec<PatchEntry>> {
    parse(region, content_type, body).map_err(|rejection| AppError::PatchListRejected {
        region,
        reason: rejection.to_string(),
    })
}

/// 依清單核對 patch 內容：總長須等於 `length`，依 `block_size` 切段（最後一段只含實際
/// bytes），每段 SHA1 須等於 `hashes[i]`（protocols §5.2）。吃 reader 而非路徑，讓呼叫端
/// 能用同一個 handle 核對與套用。
pub fn verify_blocks<R: Read>(
    reader: R,
    length: u64,
    block_size: u64,
    hashes: &[String],
) -> AppResult<()> {
    check_blocks(reader, length, block_size, hashes).map_err(|err| match err {
        VerifyError::Io(e) => AppError::io("讀取 patch 檔", e),
        mismatch => AppError::PatchVerifyFailed {
            reason: mismatch.to_string(),
        },
    })
}

/// 各區服的 patch host（protocols §5.1，兩服皆以實際清單確認）。不符時 [`Rejection::Host`]
/// 帶出清單中的實際 host，整批拒絕。
fn patch_host(region: Region) -> &'static str {
    match region {
        Region::Tw => "patch-dl.ffxiv.com.tw",
        Region::Global => "patch-dl.ffxiv.com",
    }
}

/// 清單 URL 的 scheme／host 規則，回傳實際下載用的 URL（protocols §5.1）：
/// - 台服：host 須為 [`patch_host`]，`http`、`https` 都接受，一律改成 `https`。清單經 HTTP 取得、沒有獨立
///   信任根；下載改走 TLS 後，被改過的清單只能指向官方主機上的檔案，假的 hash 只會讓核對失敗。
/// - 國際服：host 須為 [`patch_host`]、scheme 須為 `http`，原樣下載（`https://patch-dl.ffxiv.com` 不通；
///   hash 來自 HTTPS 的 session 註冊）。
///
/// 不符時回清單中的 `scheme://host`（供 [`Rejection::Host`]）。
fn download_url(region: Region, mut url: Url) -> Result<Url, String> {
    let scheme_ok = match region {
        Region::Tw => matches!(url.scheme(), "http" | "https"),
        Region::Global => url.scheme() == "http",
    };
    if !scheme_ok || url.host_str() != Some(patch_host(region)) {
        return Err(format!("{}://{}", url.scheme(), url.host_str().unwrap_or("")));
    }
    if region == Region::Tw && url.set_scheme("https").is_err() {
        // http、https 之間互換不會失敗（兩者都是 special scheme）；保險起見仍失敗關閉。
        return Err(format!("{}://{}", url.scheme(), url.host_str().unwrap_or("")));
    }
    Ok(url)
}

/// 拒絕原因；`line` 為第幾筆 patch 行（1 起算）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rejection {
    ContentType,
    Encoding,
    Unterminated,
    NoEntries,
    FieldCount { line: usize, count: usize },
    Length { line: usize },
    HashType { line: usize },
    BlockSize { line: usize },
    HashCount { line: usize, expected: u64, actual: usize },
    HashFormat { line: usize },
    VersionId { line: usize },
    Url { line: usize },
    /// `found`＝清單中的 `scheme://host`，供確認實際 patch host（非 secret）。
    Host { line: usize, found: String },
    Repo { line: usize },
    FileName { line: usize },
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rejection::ContentType => {
                f.write_str("Content-Type 不是帶 boundary 的 multipart/mixed")
            }
            Rejection::Encoding => f.write_str("回應不是 UTF-8"),
            Rejection::Unterminated => f.write_str("multipart 缺少結束 boundary"),
            Rejection::NoEntries => f.write_str("沒有任何 patch 行"),
            Rejection::FieldCount { line, count } => {
                write!(f, "第 {line} 筆 patch 有 {count} 欄，應為 9 欄")
            }
            Rejection::Length { line } => write!(f, "第 {line} 筆 patch 長度不是正整數"),
            Rejection::HashType { line } => write!(f, "第 {line} 筆 patch 的 hashType 不是 sha1"),
            Rejection::BlockSize { line } => write!(
                f,
                "第 {line} 筆 patch 的 hashBlockSize 不在 {MIN_BLOCK_SIZE}–{MAX_BLOCK_SIZE}"
            ),
            Rejection::HashCount {
                line,
                expected,
                actual,
            } => write!(f, "第 {line} 筆 patch 有 {actual} 個 hash，應為 {expected} 個"),
            Rejection::HashFormat { line } => {
                write!(f, "第 {line} 筆 patch 的 hash 不是 40 位小寫 hex")
            }
            Rejection::VersionId { line } => write!(f, "第 {line} 筆 patch 的 versionId 格式不符"),
            Rejection::Url { line } => write!(f, "第 {line} 筆 patch 的 URL 無法解析"),
            Rejection::Host { line, found } => write!(
                f,
                "第 {line} 筆 patch 的 URL（{found}）不是該區服允許的 patch 位址（host 或 scheme 不符）"
            ),
            Rejection::Repo { line } => write!(f, "第 {line} 筆 patch 的 URL 路徑判定不出 repo"),
            Rejection::FileName { line } => {
                write!(f, "第 {line} 筆 patch 的檔名不是單一安全路徑成分")
            }
        }
    }
}

fn parse(region: Region, content_type: &str, body: &[u8]) -> Result<Vec<PatchEntry>, Rejection> {
    let boundary = multipart_boundary(content_type).ok_or(Rejection::ContentType)?;
    let text = std::str::from_utf8(body).map_err(|_| Rejection::Encoding)?;
    let lines = part_lines(text, boundary)?;
    if lines.is_empty() {
        return Err(Rejection::NoEntries);
    }
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| parse_line(line, i + 1, region))
        .collect()
}

/// `multipart/mixed; boundary=…` 的 boundary（可加引號）；不是 multipart/mixed 或缺 boundary 回 `None`。
fn multipart_boundary(content_type: &str) -> Option<&str> {
    let mut params = content_type.split(';');
    let mime = params.next()?.trim();
    if !mime.eq_ignore_ascii_case("multipart/mixed") {
        return None;
    }
    params
        .find_map(|param| {
            let (key, value) = param.split_once('=')?;
            key.trim()
                .eq_ignore_ascii_case("boundary")
                .then(|| value.trim().trim_matches('"'))
        })
        .filter(|b| !b.is_empty() && b.len() <= MAX_BOUNDARY_LEN)
}

/// 取出所有 part 內容中的非空行。part header 與 preamble／epilogue 不看；
/// 開了 part 卻沒有結束 boundary（回應被截斷）即拒絕。
fn part_lines<'a>(text: &'a str, boundary: &str) -> Result<Vec<&'a str>, Rejection> {
    enum State {
        Preamble,
        Headers,
        Content,
        Closed,
    }
    let delimiter = format!("--{boundary}");
    let close = format!("--{boundary}--");
    let mut state = State::Preamble;
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line == close {
            state = State::Closed;
            break;
        }
        if line == delimiter {
            state = State::Headers;
            continue;
        }
        match state {
            State::Preamble | State::Closed => {}
            State::Headers => {
                if line.is_empty() {
                    state = State::Content;
                }
            }
            State::Content => {
                if !line.is_empty() {
                    lines.push(line);
                }
            }
        }
    }
    match state {
        State::Preamble | State::Closed => Ok(lines),
        State::Headers | State::Content => Err(Rejection::Unterminated),
    }
}

/// 9 欄：`[0]` 長度、`[1]–[3]` 未使用（不驗證）、`[4]` versionId、`[5]` hashType、
/// `[6]` hashBlockSize、`[7]` hashes、`[8]` url。
fn parse_line(line_text: &str, line: usize, region: Region) -> Result<PatchEntry, Rejection> {
    let fields: Vec<&str> = line_text.split('\t').collect();
    let &[length, _, _, _, version_id, hash_type, block_size, hashes, url] = fields.as_slice()
    else {
        return Err(Rejection::FieldCount {
            line,
            count: fields.len(),
        });
    };

    let length = positive_int(length).ok_or(Rejection::Length { line })?;
    if hash_type != "sha1" {
        return Err(Rejection::HashType { line });
    }
    let hash_block_size = positive_int(block_size)
        .filter(|n| (MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(n))
        .ok_or(Rejection::BlockSize { line })?;
    let hashes: Vec<String> = hashes.split(',').map(str::to_string).collect();
    let expected = length.div_ceil(hash_block_size);
    if hashes.len() as u64 != expected {
        return Err(Rejection::HashCount {
            line,
            expected,
            actual: hashes.len(),
        });
    }
    if !hashes.iter().all(|h| is_sha1_hex(h)) {
        return Err(Rejection::HashFormat { line });
    }
    if !is_version_id(version_id) {
        return Err(Rejection::VersionId { line });
    }

    let url = Url::parse(url).map_err(|_| Rejection::Url { line })?;
    let url = download_url(region, url).map_err(|found| Rejection::Host { line, found })?;
    let segments: Vec<&str> = url.path_segments().map(Iterator::collect).unwrap_or_default();
    let (repo, file_name) = match segments.as_slice() {
        ["game", dir, file_name] if is_hash_dir(dir) => (Some(Repo::Game), *file_name),
        ["game", ex, dir, file_name] if is_hash_dir(dir) => (expansion_of(ex), *file_name),
        _ => (None, ""),
    };
    let repo = repo.ok_or(Rejection::Repo { line })?;
    if !is_safe_file_name(file_name) {
        return Err(Rejection::FileName { line });
    }
    let file_name = file_name.to_string();

    Ok(PatchEntry {
        length,
        version_id: version_id.to_string(),
        hash_block_size,
        hashes,
        url,
        repo,
        file_name,
    })
}

/// 只接受 ASCII 十進位數字（不接受正負號），且大於 0。
fn positive_int(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u64>().ok().filter(|&n| n > 0)
}

fn is_sha1_hex(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `^\d{4}\.\d{2}\.\d{2}\.\d{4}\.\d{4}$`，`\d` 限 ASCII。
pub(crate) fn is_version_id(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 20
        && bytes.iter().enumerate().all(|(i, &b)| match i {
            4 | 7 | 10 | 15 => b == b'.',
            _ => b.is_ascii_digit(),
        })
}

/// `/game/` 與 `/game/exN/` 之後的目錄段：8 位小寫 hex（protocols §5.1）。
fn is_hash_dir(segment: &str) -> bool {
    segment.len() == 8 && segment.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `ex1`–`ex5`；其他（`ex6`、`EX1`…）不是可識別的 repo。
fn expansion_of(segment: &str) -> Option<Repo> {
    match segment {
        "ex1" => Some(Repo::Expansion(1)),
        "ex2" => Some(Repo::Expansion(2)),
        "ex3" => Some(Repo::Expansion(3)),
        "ex4" => Some(Repo::Expansion(4)),
        "ex5" => Some(Repo::Expansion(5)),
        _ => None,
    }
}

/// 只允許 `[A-Za-z0-9._-]`、以 `.patch` 結尾、不含 `..`；字元集已排除 `/`、`\`、`:`。
fn is_safe_file_name(name: &str) -> bool {
    name.len() > PATCH_SUFFIX.len()
        && name.ends_with(PATCH_SUFFIX)
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[derive(Debug)]
enum VerifyError {
    Io(io::Error),
    /// 長度、區塊大小與 hash 個數彼此不一致（呼叫端傳入的資料有誤）。
    Params,
    Short { expected: u64, actual: u64 },
    Long { expected: u64 },
    /// 0 起算的段號。
    Block { index: usize },
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerifyError::Io(e) => write!(f, "讀取失敗：{e}"),
            VerifyError::Params => f.write_str("長度、區塊大小與 hash 個數不一致"),
            VerifyError::Short { expected, actual } => {
                write!(f, "檔案長度 {actual}，清單為 {expected}")
            }
            VerifyError::Long { expected } => write!(f, "檔案長度超過清單的 {expected}"),
            VerifyError::Block { index } => write!(f, "第 {} 段 SHA1 不符", index + 1),
        }
    }
}

/// 長度不足立即回報；hash 不符先記下第一段，讀完確認長度後再回報（長度錯誤優先）。
fn check_blocks<R: Read>(
    mut reader: R,
    length: u64,
    block_size: u64,
    hashes: &[String],
) -> Result<(), VerifyError> {
    if block_size == 0 || length == 0 || length.div_ceil(block_size) != hashes.len() as u64 {
        return Err(VerifyError::Params);
    }
    let mut buf = vec![0u8; READ_BUF_SIZE];
    let mut total = 0u64;
    let mut first_mismatch = None;
    for (index, expected_hash) in hashes.iter().enumerate() {
        let want = block_size.min(length - total);
        let mut hasher = Sha1::new();
        let mut got = 0u64;
        while got < want {
            let cap = usize::try_from(want - got).map_or(buf.len(), |rest| rest.min(buf.len()));
            let n = read_some(&mut reader, &mut buf[..cap]).map_err(VerifyError::Io)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            got += n as u64;
        }
        total += got;
        if got < want {
            return Err(VerifyError::Short {
                expected: length,
                actual: total,
            });
        }
        if first_mismatch.is_none() && hex::encode(hasher.finalize()) != *expected_hash {
            first_mismatch = Some(index);
        }
    }
    if read_some(&mut reader, &mut buf[..1]).map_err(VerifyError::Io)? != 0 {
        return Err(VerifyError::Long { expected: length });
    }
    match first_mismatch {
        Some(index) => Err(VerifyError::Block { index }),
        None => Ok(()),
    }
}

/// `Read::read`，遇 `Interrupted` 重試。
fn read_some<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match reader.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 台服實際回應（2026-10-13 14:36 UTC，HTTP 200）：game 2026.08.07＋ex1 2026.07.03 的版本檢查。
    const TW_BODY: &[u8] = include_bytes!("testdata/tw_patchlist_2026-10-13.bin");
    const TW_CONTENT_TYPE: &str = include_str!("testdata/tw_patchlist_2026-10-13.ct");
    /// 清單第 7 筆（ex1 2026.08.07）的實際 patch 檔，52,848 bytes。
    const TW_EX1_PATCH: &[u8] = include_bytes!("testdata/D2026.08.07.0000.0000.patch");

    const LINE1_HASHES: &str =
        "04b7c4397fd07ebcc2a18670d457526a2f98187d,92763a37e0486263f1d39d87da878aef8402ee9a";
    const EX1_URL: &str =
        "http://patch-dl.ffxiv.com.tw/game/ex1/be3c0f25/D2026.08.07.0000.0000.patch";

    fn tw_text() -> &'static str {
        std::str::from_utf8(TW_BODY).unwrap()
    }

    /// 把實際 fixture 的第一處 `from` 改成 `to`。
    fn tw_with(from: &str, to: &str) -> String {
        assert!(tw_text().contains(from), "fixture 不含 {from:?}");
        tw_text().replacen(from, to, 1)
    }

    fn parse_tw(body: &str) -> Result<Vec<PatchEntry>, Rejection> {
        parse(Region::Tw, TW_CONTENT_TYPE.trim(), body.as_bytes())
    }

    fn reject(body: &str) -> Rejection {
        parse_tw(body).unwrap_err()
    }

    fn block_hashes(data: &[u8], block_size: usize) -> Vec<String> {
        data.chunks(block_size)
            .map(|c| hex::encode(Sha1::digest(c)))
            .collect()
    }

    #[test]
    fn parses_real_tw_response() {
        let entries = parse_patch_list(Region::Tw, TW_CONTENT_TYPE.trim(), TW_BODY).unwrap();
        let versions: Vec<(&str, Repo)> = entries
            .iter()
            .map(|e| (e.version_id.as_str(), e.repo))
            .collect();
        assert_eq!(
            versions,
            [
                ("2026.08.17.0000.0001", Repo::Game),
                ("2026.08.24.0000.0000", Repo::Game),
                ("2026.08.31.0000.0000", Repo::Game),
                ("2026.09.07.0000.0000", Repo::Game),
                ("2026.09.09.0000.0000", Repo::Game),
                ("2026.09.14.0000.0000", Repo::Game),
                ("2026.08.07.0000.0000", Repo::Expansion(1)),
                ("2026.08.10.0000.0000", Repo::Expansion(1)),
                ("2026.08.31.0000.0000", Repo::Expansion(1)),
            ]
        );

        let first = &entries[0];
        assert_eq!(first.length, 74_892_191);
        assert_eq!(first.hash_block_size, 50_000_000);
        assert_eq!(
            first.hashes,
            [
                "04b7c4397fd07ebcc2a18670d457526a2f98187d",
                "92763a37e0486263f1d39d87da878aef8402ee9a"
            ]
        );
        // 清單是 http，實際下載位址改成 https。
        assert_eq!(
            first.url.as_str(),
            "https://patch-dl.ffxiv.com.tw/game/0b90d03e/D2026.08.17.0000.0001.patch"
        );
        assert_eq!(first.file_name, "D2026.08.17.0000.0001.patch");

        let ex1 = &entries[6];
        assert_eq!(ex1.length, 52_848);
        assert_eq!(ex1.hash_block_size, 50_000_000);
        assert_eq!(ex1.hashes, ["625499a52faf29e527543ee969e17ff9a81edcb6"]);
        assert_eq!(ex1.url.as_str(), EX1_URL.replacen("http://", "https://", 1));
        assert_eq!(ex1.file_name, "D2026.08.07.0000.0000.patch");
    }

    #[test]
    fn real_ex1_patch_passes_block_verification() {
        let entries = parse_tw(tw_text()).unwrap();
        let ex1 = &entries[6];
        assert_eq!(ex1.file_name, "D2026.08.07.0000.0000.patch");
        verify_blocks(TW_EX1_PATCH, ex1.length, ex1.hash_block_size, &ex1.hashes).unwrap();
    }

    #[test]
    fn rejection_reaches_app_error_without_echoing_list() {
        let body = tw_with("\tsha1\t50000000\t625499a5", "\tmd5\t50000000\t625499a5");
        let err = parse_patch_list(Region::Tw, TW_CONTENT_TYPE.trim(), body.as_bytes()).unwrap_err();
        assert!(matches!(
            err,
            AppError::PatchListRejected {
                region: Region::Tw,
                ..
            }
        ));
        let message = err.to_string();
        assert!(!message.contains("http"), "{message}");
        assert!(!message.contains("patch-dl"), "{message}");
    }

    #[test]
    fn rejects_when_no_patch_lines() {
        assert_eq!(reject(""), Rejection::NoEntries);
        let empty_part = "--477D80B1_38BC_41d4_8B48_5273ADB89CAC\r\n\
                          Content-Type: application/octet-stream\r\n\r\n\r\n\
                          --477D80B1_38BC_41d4_8B48_5273ADB89CAC--\r\n";
        assert_eq!(reject(empty_part), Rejection::NoEntries);
    }

    #[test]
    fn rejects_field_count_other_than_nine() {
        // 去掉 hashes 欄（不改走無 hash 分支）。
        let eight = tw_with(&format!("\t{LINE1_HASHES}\t"), "\t");
        assert_eq!(reject(&eight), Rejection::FieldCount { line: 1, count: 8 });
        let ten = tw_with(EX1_URL, &format!("{EX1_URL}\textra"));
        assert_eq!(reject(&ten), Rejection::FieldCount { line: 7, count: 10 });
    }

    #[test]
    fn rejects_length_not_positive_integer() {
        for bad in ["0", "-52848", "+52848", "52848.0", "", "５2848"] {
            let body = tw_with("52848\t6112064028", &format!("{bad}\t6112064028"));
            assert_eq!(reject(&body), Rejection::Length { line: 7 }, "{bad:?}");
        }
    }

    #[test]
    fn rejects_hash_type_other_than_sha1() {
        for bad in ["md5", "SHA1", "sha256", ""] {
            let body = tw_with(
                "\tsha1\t50000000\t625499a5",
                &format!("\t{bad}\t50000000\t625499a5"),
            );
            assert_eq!(reject(&body), Rejection::HashType { line: 7 }, "{bad:?}");
        }
    }

    #[test]
    fn rejects_block_size_out_of_range() {
        for bad in ["1048575", "268435457", "0", "-1", "abc"] {
            let body = tw_with(
                "\tsha1\t50000000\t625499a5",
                &format!("\tsha1\t{bad}\t625499a5"),
            );
            assert_eq!(reject(&body), Rejection::BlockSize { line: 7 }, "{bad:?}");
        }
        // 上下限本身可接受（52,848 bytes 仍是 1 段）。
        for ok in ["1048576", "268435456"] {
            let body = tw_with(
                "\tsha1\t50000000\t625499a5",
                &format!("\tsha1\t{ok}\t625499a5"),
            );
            assert_eq!(parse_tw(&body).unwrap()[6].hash_block_size.to_string(), ok);
        }
    }

    #[test]
    fn rejects_hash_count_not_matching_blocks() {
        let missing = tw_with(LINE1_HASHES, "04b7c4397fd07ebcc2a18670d457526a2f98187d");
        assert_eq!(
            reject(&missing),
            Rejection::HashCount {
                line: 1,
                expected: 2,
                actual: 1
            }
        );
        let extra = tw_with(
            "625499a52faf29e527543ee969e17ff9a81edcb6",
            "625499a52faf29e527543ee969e17ff9a81edcb6,625499a52faf29e527543ee969e17ff9a81edcb6",
        );
        assert_eq!(
            reject(&extra),
            Rejection::HashCount {
                line: 7,
                expected: 1,
                actual: 2
            }
        );
        // blockSize 改大後同一組 hash 個數不再相符。
        let larger_blocks = tw_with(
            &format!("\tsha1\t50000000\t{LINE1_HASHES}"),
            &format!("\tsha1\t268435456\t{LINE1_HASHES}"),
        );
        assert_eq!(
            reject(&larger_blocks),
            Rejection::HashCount {
                line: 1,
                expected: 1,
                actual: 2
            }
        );
    }

    #[test]
    fn rejects_hash_not_lowercase_sha1_hex() {
        for bad in [
            "625499A52FAF29E527543EE969E17FF9A81EDCB6",
            "625499a52faf29e527543ee969e17ff9a81edcb",
            "625499a52faf29e527543ee969e17ff9a81edcbg",
            "",
        ] {
            let body = tw_with("625499a52faf29e527543ee969e17ff9a81edcb6", bad);
            assert_eq!(reject(&body), Rejection::HashFormat { line: 7 }, "{bad:?}");
        }
    }

    #[test]
    fn rejects_malformed_version_id() {
        for bad in [
            "2026.08.07.0000.000",
            "2026.8.07.0000.00000",
            "2026-08-07.0000.0000",
            "２026.08.07.0000.0000",
            "2026.08.07.0000.0000 ",
            "",
        ] {
            let body = tw_with(
                "\t2026.08.07.0000.0000\tsha1",
                &format!("\t{bad}\tsha1"),
            );
            assert_eq!(reject(&body), Rejection::VersionId { line: 7 }, "{bad:?}");
        }
    }

    #[test]
    fn rejects_unsafe_file_name() {
        for bad in [
            "D2026.08.07..patch",
            "D2026:08.patch",
            "D2026.08.07.0000.0000.zip",
            "D2026.08.07.0000.0000.patch.exe",
            "D%2e%2e.patch",
            "D 2026.patch",
            ".patch",
        ] {
            let body = tw_with(
                "be3c0f25/D2026.08.07.0000.0000.patch",
                &format!("be3c0f25/{bad}"),
            );
            assert_eq!(reject(&body), Rejection::FileName { line: 7 }, "{bad:?}");
        }
        // 空檔名（URL 以 / 結尾）。
        let body = tw_with("be3c0f25/D2026.08.07.0000.0000.patch", "be3c0f25/");
        assert_eq!(reject(&body), Rejection::FileName { line: 7 });
    }

    #[test]
    fn tw_http_and_https_urls_download_over_https() {
        let from_http = parse_tw(tw_text()).unwrap();
        assert!(from_http.iter().all(|e| e.url.scheme() == "https"
            && e.url.host_str() == Some("patch-dl.ffxiv.com.tw")
            && e.url.port().is_none()));
        // 清單本來就是 https：照樣接受，結果與 http 清單相同。
        let https = tw_text().replace("http://patch-dl.ffxiv.com.tw/", "https://patch-dl.ffxiv.com.tw/");
        assert_ne!(https, tw_text());
        assert_eq!(parse_tw(&https).unwrap(), from_http);
    }

    #[test]
    fn global_urls_stay_http_as_listed() {
        let global = tw_text().replace("http://patch-dl.ffxiv.com.tw/", "http://patch-dl.ffxiv.com/");
        let entries = parse(Region::Global, TW_CONTENT_TYPE.trim(), global.as_bytes()).unwrap();
        assert_eq!(entries.len(), 9);
        assert_eq!(
            entries[6].url.as_str(),
            "http://patch-dl.ffxiv.com/game/ex1/be3c0f25/D2026.08.07.0000.0000.patch"
        );
        // 國際服規則不變：只接受 http（`https://patch-dl.ffxiv.com` 不通）。
        let https = global.replacen("http://patch-dl.ffxiv.com/", "https://patch-dl.ffxiv.com/", 1);
        assert_eq!(
            parse(Region::Global, TW_CONTENT_TYPE.trim(), https.as_bytes()),
            Err(Rejection::Host { line: 1, found: "https://patch-dl.ffxiv.com".into() })
        );
    }

    #[test]
    fn rejects_foreign_host() {
        for (bad, found) in [
            ("http://evil.example/game/ex1/be3c0f25/D2026.08.07.0000.0000.patch", "http://evil.example"),
            ("https://evil.example/game/ex1/be3c0f25/D2026.08.07.0000.0000.patch", "https://evil.example"),
            (
                "http://patch-dl.ffxiv.com.tw.evil.example/game/ex1/be3c0f25/D2026.08.07.0000.0000.patch",
                "http://patch-dl.ffxiv.com.tw.evil.example",
            ),
            (
                "http://patch-dl.ffxiv.com.tw@evil.example/game/ex1/be3c0f25/D2026.08.07.0000.0000.patch",
                "http://evil.example",
            ),
            ("http://patch-dl.ffxiv.com/game/ex1/be3c0f25/D2026.08.07.0000.0000.patch", "http://patch-dl.ffxiv.com"),
            ("file:///game/ex1/be3c0f25/D2026.08.07.0000.0000.patch", "file://"),
            (
                "ftp://patch-dl.ffxiv.com.tw/game/ex1/be3c0f25/D2026.08.07.0000.0000.patch",
                "ftp://patch-dl.ffxiv.com.tw",
            ),
        ] {
            let body = tw_with(EX1_URL, bad);
            assert_eq!(reject(&body), Rejection::Host { line: 7, found: found.into() }, "{bad:?}");
        }
        let body = tw_with(EX1_URL, "not a url");
        assert_eq!(reject(&body), Rejection::Url { line: 7 });
    }

    #[test]
    fn rejects_other_regions_host() {
        assert_eq!(
            parse(Region::Global, TW_CONTENT_TYPE.trim(), TW_BODY),
            Err(Rejection::Host { line: 1, found: "http://patch-dl.ffxiv.com.tw".into() })
        );
    }

    #[test]
    fn rejects_url_path_without_repo() {
        for bad in [
            "/patch/ex1/be3c0f25/D2026.08.07.0000.0000.patch",
            "/game/D2026.08.07.0000.0000.patch",
            "/game/ex_1/be3c0f25/D2026.08.07.0000.0000.patch",
            "/game//D2026.08.07.0000.0000.patch",
            "/ffxiv/game/ex1/D2026.08.07.0000.0000.patch",
            "/game/ex6/be3c0f25/D2026.08.07.0000.0000.patch",
            "/game/EX1/be3c0f25/D2026.08.07.0000.0000.patch",
            "/game/ex1/BE3C0F25/D2026.08.07.0000.0000.patch",
            "/game/ex1/be3c0f2/D2026.08.07.0000.0000.patch",
            "/game/ex1/be3c0f25/extra/D2026.08.07.0000.0000.patch",
            "/game/0b90d03e/ex1/D2026.08.07.0000.0000.patch",
        ] {
            let body = tw_with(
                "/game/ex1/be3c0f25/D2026.08.07.0000.0000.patch",
                bad,
            );
            assert_eq!(reject(&body), Rejection::Repo { line: 7 }, "{bad:?}");
        }
    }

    #[test]
    fn expansion_segment_is_ex1_to_ex5_only() {
        assert_eq!(expansion_of("ex1"), Some(Repo::Expansion(1)));
        assert_eq!(expansion_of("ex5"), Some(Repo::Expansion(5)));
        assert_eq!(expansion_of("ex6"), None);
        assert_eq!(expansion_of("0b90d03e"), None);
    }

    #[test]
    fn rejects_non_multipart_or_truncated_response() {
        for bad in ["text/plain", "multipart/mixed", "multipart/mixed; boundary=", ""] {
            assert_eq!(
                parse(Region::Tw, bad, TW_BODY),
                Err(Rejection::ContentType),
                "{bad:?}"
            );
        }
        let truncated = tw_with("--477D80B1_38BC_41d4_8B48_5273ADB89CAC--", "");
        assert_eq!(reject(&truncated), Rejection::Unterminated);
        assert_eq!(
            parse(Region::Tw, TW_CONTENT_TYPE.trim(), &[0xff, 0xfe]),
            Err(Rejection::Encoding)
        );
    }

    #[test]
    fn unused_metadata_fields_are_not_validated() {
        let body = tw_with("52848\t6112064028\t29\t4\t", "52848\tx\t\t-1\t");
        assert_eq!(parse_tw(&body).unwrap()[6].length, 52_848);
    }

    #[test]
    fn verify_accepts_matching_blocks() {
        let data: Vec<u8> = (0..30u8).collect();
        check_blocks(&data[..], 30, 10, &block_hashes(&data, 10)).unwrap();
    }

    #[test]
    fn verify_hashes_only_actual_bytes_of_short_last_block() {
        let data: Vec<u8> = (0..25u8).collect();
        let hashes = block_hashes(&data, 10);
        assert_eq!(hashes.len(), 3);
        check_blocks(&data[..], 25, 10, &hashes).unwrap();

        // 末段若補滿 blockSize 再 hash 就不會相符。
        let mut padded = data.clone();
        padded.resize(30, 0);
        let padded_hashes = block_hashes(&padded, 10);
        assert!(matches!(
            check_blocks(&data[..], 25, 10, &padded_hashes),
            Err(VerifyError::Block { index: 2 })
        ));
    }

    #[test]
    fn verify_rejects_length_mismatch() {
        let data: Vec<u8> = (0..25u8).collect();
        let hashes = block_hashes(&data, 10);
        assert!(matches!(
            check_blocks(&data[..24], 25, 10, &hashes),
            Err(VerifyError::Short {
                expected: 25,
                actual: 24
            })
        ));
        let mut longer = data.clone();
        longer.push(0);
        assert!(matches!(
            check_blocks(&longer[..], 25, 10, &hashes),
            Err(VerifyError::Long { expected: 25 })
        ));
        assert!(matches!(
            verify_blocks(&longer[..], 25, 10, &hashes),
            Err(AppError::PatchVerifyFailed { .. })
        ));
    }

    #[test]
    fn verify_rejects_block_hash_mismatch() {
        let data: Vec<u8> = (0..25u8).collect();
        let hashes = block_hashes(&data, 10);
        let mut tampered = data.clone();
        tampered[12] ^= 0xff;
        assert!(matches!(
            check_blocks(&tampered[..], 25, 10, &hashes),
            Err(VerifyError::Block { index: 1 })
        ));
    }

    #[test]
    fn verify_rejects_inconsistent_params() {
        let data: Vec<u8> = (0..25u8).collect();
        let hashes = block_hashes(&data, 10);
        assert!(matches!(
            check_blocks(&data[..], 25, 10, &hashes[..2]),
            Err(VerifyError::Params)
        ));
        assert!(matches!(
            check_blocks(&data[..], 25, 0, &hashes),
            Err(VerifyError::Params)
        ));
    }
}
