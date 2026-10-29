//! 角色資料檔格式（protocols §7.2）：二進位檔的完整性判斷，與純文字檔以 key 合併。

use std::collections::{HashMap, HashSet};

/// 二進位檔頭的最小長度（libxivdat：0x11 bytes）。
const HEADER_LEN: usize = 0x11;
/// 檔案總長＝最大內容長度＋這個值（protocols §7.2）。
const LEN_OVERHEAD: usize = 32;

/// 完整的二進位檔回傳前 8 bytes（類型＋最大內容長度），否則回不轉的原因。完整＝長度至少 17 bytes、
/// 總長＝最大內容長度＋32、內容長度不大於最大內容長度（protocols §7.3）。
pub fn binary_prefix(bytes: &[u8]) -> Result<[u8; 8], &'static str> {
    if bytes.len() < HEADER_LEN {
        return Err("檔案太短，不是完整的設定檔");
    }
    let max_len = u32_at(bytes, 4) as usize;
    let content_len = u32_at(bytes, 8) as usize;
    if max_len.checked_add(LEN_OVERHEAD) != Some(bytes.len()) {
        return Err("檔案長度與檔頭不符");
    }
    if content_len > max_len {
        return Err("檔頭的內容長度超過上限");
    }
    let mut prefix = [0; 8];
    prefix.copy_from_slice(&bytes[..8]);
    Ok(prefix)
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// 純文字檔合併的結果。
#[derive(Debug, PartialEq, Eq)]
pub struct Merged {
    pub bytes: Vec<u8>,
    /// 來源有、目標沒有，這次不加的 key 數（以（區段、key、第幾次出現）計）。
    pub missing_keys: usize,
}

/// （區段標頭、key、該 key 在區段內第幾次出現）；區段標頭前的行區段為 `None`。
type Slot<'a> = (Option<&'a [u8]>, &'a [u8], usize);

/// 以 `target` 的原始 bytes 為底，只把值換成 `source` 同一個 [`Slot`] 的值（原始 bytes，不轉碼）；其他 bytes
/// （區段標頭、沒有 `\t` 的行、換行、檔尾 `\x00`）原樣保留，來源有、目標沒有的 key 不加（protocols §7.3）。
pub fn merge_text(target: &[u8], source: &[u8]) -> Merged {
    let source_values: HashMap<Slot, &[u8]> =
        entries(source).into_iter().map(|e| (e.slot, &source[e.value.clone()])).collect();
    let target_entries = entries(target);
    let target_slots: HashSet<Slot> = target_entries.iter().map(|e| e.slot).collect();
    let missing_keys = source_values.keys().filter(|slot| !target_slots.contains(*slot)).count();
    let mut bytes = Vec::with_capacity(target.len());
    let mut copied = 0;
    for entry in &target_entries {
        if let Some(value) = source_values.get(&entry.slot) {
            bytes.extend_from_slice(&target[copied..entry.value.start]);
            bytes.extend_from_slice(value);
            copied = entry.value.end;
        }
    }
    bytes.extend_from_slice(&target[copied..]);
    Merged { bytes, missing_keys }
}

struct Entry<'a> {
    slot: Slot<'a>,
    /// 值在原文中的範圍：`\t` 之後到行尾（不含換行與結尾的 `\x00`）。
    value: std::ops::Range<usize>,
}

fn entries(text: &[u8]) -> Vec<Entry<'_>> {
    let mut out = Vec::new();
    let mut section: Option<&[u8]> = None;
    let mut seen: HashMap<(Option<&[u8]>, &[u8]), usize> = HashMap::new();
    let mut start = 0;
    while start < text.len() {
        let end = text[start..].iter().position(|&b| b == b'\n').map_or(text.len(), |i| start + i);
        let mut line_end = end;
        while line_end > start && matches!(text[line_end - 1], b'\r' | 0) {
            line_end -= 1;
        }
        let line = &text[start..line_end];
        if line.first() == Some(&b'<') && line.last() == Some(&b'>') {
            section = Some(line);
        } else if let Some(tab) = line.iter().position(|&b| b == b'\t') {
            let key = &line[..tab];
            let count = seen.entry((section, key)).or_insert(0);
            out.push(Entry {
                slot: (section, key, *count),
                value: start + tab + 1..line_end,
            });
            *count += 1;
        }
        start = end + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary(kind: u32, max_len: u32, content_len: u32, total: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; total];
        bytes[..4].copy_from_slice(&kind.to_le_bytes());
        bytes[4..8].copy_from_slice(&max_len.to_le_bytes());
        bytes[8..12].copy_from_slice(&content_len.to_le_bytes());
        bytes
    }

    #[test]
    fn complete_binary_returns_type_and_max_length() {
        let bytes = binary(0x0068_0007, 0x3B05, 0x100, 0x3B05 + 32);
        assert_eq!(binary_prefix(&bytes).unwrap(), bytes[..8]);
    }

    #[test]
    fn incomplete_binary_is_rejected() {
        // 長度與檔頭不符（實測：同機有比最大長度＋32 多出 bytes 的檔）。
        assert!(binary_prefix(&binary(7, 0x100, 0x10, 0x100 + 32 + 5)).is_err());
        assert!(binary_prefix(&binary(7, 0x100, 0x10, 0x100 + 31)).is_err());
        // 內容長度超過上限。
        assert!(binary_prefix(&binary(7, 0x100, 0x101, 0x100 + 32)).is_err());
        // 不到檔頭長度。
        assert!(binary_prefix(&[0; 16]).is_err());
        // 最大長度溢位不 panic。
        assert!(binary_prefix(&binary(7, u32::MAX, 0, 64)).is_err());
    }

    #[test]
    fn merge_replaces_values_by_section_key_and_occurrence() {
        let target = b"Top\t1\r\n<A>\r\nk\tt1\r\nk\tt2\r\nonly_t\tx\r\nnote line\r\n<B>\r\nk\tb\r\n\x00";
        let source = "Top\t9\r\n<A>\r\nk\t一\r\nk\ts2\r\nk\ts3\r\nonly_s\ty\r\n<B>\r\nk\tsb\r\n";
        let merged = merge_text(target, source.as_bytes());
        let expected = "Top\t9\r\n<A>\r\nk\t一\r\nk\ts2\r\nonly_t\tx\r\nnote line\r\n<B>\r\nk\tsb\r\n\x00";
        assert_eq!(merged.bytes, expected.as_bytes());
        // 來源多出的 <A> 第 3 個 k 與 only_s 不加。
        assert_eq!(merged.missing_keys, 2);
    }

    #[test]
    fn merge_keeps_target_tail_with_or_without_nul() {
        let merged = merge_text(b"<A>\r\nk\told", b"<A>\r\nk\tnew\r\n\x00");
        assert_eq!(merged.bytes, b"<A>\r\nk\tnew");
        let merged = merge_text(b"<A>\r\nk\told\x00", b"<A>\r\nk\tnew");
        assert_eq!(merged.bytes, b"<A>\r\nk\tnew\x00");
        assert_eq!(merged.missing_keys, 0);
    }

    #[test]
    fn same_key_in_other_section_is_a_different_slot() {
        let merged = merge_text(b"<A>\r\nk\t1\r\n", b"<B>\r\nk\t2\r\n");
        assert_eq!(merged.bytes, b"<A>\r\nk\t1\r\n");
        assert_eq!(merged.missing_keys, 1);
    }
}
