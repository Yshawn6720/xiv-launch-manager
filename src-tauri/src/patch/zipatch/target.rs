//! 輸出路徑（protocols §5.4.5）：SqPack 路徑推導、SQPK/F／ADIR／DELD 路徑正規化，以及寫入前的
//! junction／symlink 檢查。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::{Fail, Rule};
use crate::game::MAX_EXPANSION;

/// Win32 的 DOS 裝置名：路徑成分（不分大小寫、忽略副檔名）是其中之一時會開到裝置而不是檔案。
const DOS_DEVICES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// SQPK 的 SqPack 目標；建構時已確認 ExpansionId（`SubId >> 8`）不大於 5。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SqpackTarget {
    main: u16,
    sub: u16,
    file: u32,
}

impl SqpackTarget {
    pub(super) fn new(main: u16, sub: u16, file: u32) -> Result<Self, Rule> {
        if (sub >> 8) > u16::from(MAX_EXPANSION) {
            return Err(Rule::Expansion);
        }
        Ok(Self { main, sub, file })
    }

    /// `sqpack\{資料夾}\{主幹}.dat{FileId}`。
    pub(super) fn dat(self) -> PathBuf {
        self.in_folder(format!("{}.dat{}", self.stem(), self.file))
    }

    /// FileId 0 為 `sqpack\{資料夾}\{主幹}.index`，否則 `{主幹}.index{FileId}`。
    pub(super) fn index(self) -> PathBuf {
        let stem = self.stem();
        self.in_folder(match self.file {
            0 => format!("{stem}.index"),
            n => format!("{stem}.index{n}"),
        })
    }

    /// 主幹 `{MainId:x2}{SubId:x4}.win32`。本專案只接受 platform 0（SQPK/T），恆為 win32。
    fn stem(self) -> String {
        format!("{:02x}{:04x}.win32", self.main, self.sub)
    }

    /// 資料夾：ExpansionId 0＝`ffxiv`，N＝`exN`。
    fn in_folder(self, name: String) -> PathBuf {
        let folder = match self.sub >> 8 {
            0 => "ffxiv".to_string(),
            n => format!("ex{n}"),
        };
        PathBuf::from("sqpack").join(folder).join(name)
    }
}

/// SQPK/F、ADIR、DELD 的路徑：`/`、`\` 都是分隔符，去除前導分隔符後視為相對套用根目錄。
/// 任一成分為空、以 `.` 或空白結尾（含 `.`、`..`；Win32 會去掉成分尾端的 `.` 與空白，
/// 使實際路徑與字面不同）、含 `:`（磁碟代號、ADS），或是 DOS 裝置名，即拒絕。因此結果一定
/// 落在根目錄之內，也不會是根目錄本身（空字串只有一個空成分）。
pub(super) fn relative_path(raw: &str) -> Option<PathBuf> {
    let mut path = PathBuf::new();
    for part in raw.trim_start_matches(['/', '\\']).split(['/', '\\']) {
        if part.is_empty()
            || part.ends_with(['.', ' '])
            || part.contains(':')
            || is_dos_device(part)
        {
            return None;
        }
        path.push(part);
    }
    Some(path)
}

/// 第一個 `.` 之前（去掉尾端空白）的部分不分大小寫等於 DOS 裝置名，例如 `nul`、`CON.txt`、`aux .log`。
pub(crate) fn is_dos_device(part: &str) -> bool {
    let stem = part.split_once('.').map_or(part, |(stem, _)| stem).trim_end_matches(' ');
    DOS_DEVICES.iter().any(|device| device.eq_ignore_ascii_case(stem))
}

/// 套用根目錄必須是既有目錄，且本身不是 junction／symlink。
pub(super) fn ensure_root(root: &Path) -> Result<(), Fail> {
    match fs::symlink_metadata(root) {
        Ok(meta) if is_link(&meta) => Err(Fail::reject(Rule::Link)),
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(Fail::reject(Rule::Root)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(Fail::reject(Rule::Root)),
        Err(e) => Err(Fail::io("檢查套用根目錄", e)),
    }
}

/// 從根目錄往下逐層檢查 `rel` 經過的既有目錄與目標本身：任一層是 junction／symlink 即拒絕。
/// 遇到不存在的一層就停（更深的層不可能存在）；中間一層是檔案也停，實際操作會自行失敗。
pub(super) fn ensure_no_link(root: &Path, rel: &Path) -> Result<(), Fail> {
    ensure_root(root)?;
    let mut path = root.to_path_buf();
    for part in rel.components() {
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(meta) if is_link(&meta) => return Err(Fail::reject(Rule::Link)),
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(Fail::io("檢查輸出路徑", e)),
        }
    }
    Ok(())
}

/// 連結類（name-surrogate）reparse point：junction、symlink。Rust std 在 Windows 上對帶
/// name-surrogate tag 的 reparse point 回報 `is_symlink`；WOF 壓縮、OneDrive 佔位檔等
/// 非連結型 reparse point 不算，照常寫入。`symlink_metadata` 不跟隨連結，看的是該層本身。
fn is_link(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}
