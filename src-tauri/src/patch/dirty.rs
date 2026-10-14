//! 遊戲更新的 dirty 標記（主 note「dirty 標記」）：套用前（第一筆寫入遊戲目錄前）由一般權限的本程式
//! 寫入並落盤，只有確定沒寫壞遊戲目錄的結果才清除；標記存在時該安裝拒絕更新與啟動，直到使用者以
//! 官方啟動器修復後在本程式確認清除。
//!
//! 檔案為 `%APPDATA%\xiv-launch-manager\state\patch_dirty.json`，以正規化後的安裝根目錄為 key。
//! 讀改寫由呼叫端經 app-wide launch mutex 串行。讀不到或格式錯誤一律回錯誤（呼叫端因此拒絕啟動與更新），
//! 不當成「沒有標記」。

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::region::Region;
use crate::store::{self, AppPaths};

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DirtyFile {
    #[serde(default)]
    installs: BTreeMap<String, DirtyEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DirtyEntry {
    /// 標記寫入時間（UTC RFC 3339），只供人工排查。
    marked_at: String,
}

/// 寫入標記，回傳時標記已落盤（見 [`save`]）。
pub fn mark(paths: &AppPaths, install: &Path) -> AppResult<()> {
    let mut data = load(paths)?;
    data.installs.insert(
        install_key(install),
        DirtyEntry {
            marked_at: chrono::Utc::now().to_rfc3339(),
        },
    );
    save(paths, &data)
}

/// 清除標記；本來就沒有視為成功。
pub fn clear(paths: &AppPaths, install: &Path) -> AppResult<()> {
    let mut data = load(paths)?;
    if data.installs.remove(&install_key(install)).is_some() {
        save(paths, &data)?;
    }
    Ok(())
}

pub fn is_dirty(paths: &AppPaths, install: &Path) -> AppResult<bool> {
    Ok(load(paths)?.installs.contains_key(&install_key(install)))
}

/// 標記存在即 [`AppError::PatchDirty`]（啟動與更新的開頭都要先過這關）。
pub fn ensure_clean(paths: &AppPaths, region: Region, install: &Path) -> AppResult<()> {
    if is_dirty(paths, install)? {
        Err(AppError::PatchDirty { region })
    } else {
        Ok(())
    }
}

fn load(paths: &AppPaths) -> AppResult<DirtyFile> {
    let file = paths.patch_dirty_file();
    match fs::read(&file) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| AppError::Io {
            context: format!("解析 {}", file.display()),
            message: e.to_string(),
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(DirtyFile::default()),
        Err(e) => Err(AppError::io(format!("讀取 {}", file.display()), e)),
    }
}

/// 以 [`store::write_json_atomic`] 寫入（暫存檔 `sync_all` 後寫穿取代）：中斷時不會留下半個 JSON；回傳時內容與
/// 檔名都已落盤。取代失敗即寫入失敗。
fn save(paths: &AppPaths, data: &DirtyFile) -> AppResult<()> {
    store::write_json_atomic(&paths.patch_dirty_file(), data)
}

/// 同一個安裝不論設定裡怎麼寫（大小寫、`/` 或 `\`、結尾分隔符、`.`／`..`、經 junction）都得到同一個 key：
/// 存在時以 canonicalize 解析並去掉 `\\?\` 前綴，否則逐成分正規化；最後統一 `\` 並轉小寫（NTFS 不分大小寫）。
fn install_key(install: &Path) -> String {
    let resolved = match fs::canonicalize(install) {
        Ok(path) => strip_verbatim(&path.to_string_lossy()),
        Err(_) => lexical(install).to_string_lossy().into_owned(),
    };
    resolved
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// `\\?\C:\x` → `C:\x`；`\\?\UNC\server\share` → `\\server\share`。
fn strip_verbatim(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
    }
}

/// 不碰檔案系統的正規化：去掉 `.`，`..` 退一層。
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, AppPaths, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().join("appdata"));
        let install = dir.path().join("Game Root");
        fs::create_dir_all(install.join("game")).expect("mkdir");
        (dir, paths, install)
    }

    #[test]
    fn mark_persists_and_clear_removes() {
        let (_dir, paths, install) = setup();
        assert!(!is_dirty(&paths, &install).expect("read"));
        assert!(ensure_clean(&paths, Region::Tw, &install).is_ok());

        mark(&paths, &install).expect("mark");
        // 另一個 AppPaths 實例（如重開程式）讀到同一份標記。
        let reopened = AppPaths::new(paths.root().to_path_buf());
        assert!(is_dirty(&reopened, &install).expect("read"));
        assert!(matches!(
            ensure_clean(&reopened, Region::Tw, &install),
            Err(AppError::PatchDirty { region: Region::Tw })
        ));
        let leftovers: Vec<_> = fs::read_dir(paths.patch_dirty_file().parent().expect("state dir"))
            .expect("list")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(leftovers, ["patch_dirty.json"], "沒有留下暫存檔");

        clear(&paths, &install).expect("clear");
        assert!(!is_dirty(&paths, &install).expect("read"));
        // 沒有標記時清除是 no-op。
        clear(&paths, &install).expect("clear again");
    }

    #[test]
    fn key_ignores_case_separators_and_dot_segments() {
        let (_dir, paths, install) = setup();
        mark(&paths, &install).expect("mark");

        let text = install.to_string_lossy().into_owned();
        let variants = [
            PathBuf::from(text.to_uppercase()),
            PathBuf::from(text.replace('\\', "/")),
            PathBuf::from(format!("{text}\\")),
            install.join(".").join("game").join(".."),
        ];
        for variant in &variants {
            assert!(is_dirty(&paths, variant).expect("read"), "{}", variant.display());
        }
        // 不存在的路徑走逐成分正規化，同樣不分大小寫與分隔符。
        let missing = install.join("Not Here");
        assert_eq!(
            install_key(&missing),
            install_key(&PathBuf::from(missing.to_string_lossy().to_lowercase().replace('\\', "/")))
        );

        let other = install.with_file_name("Other Root");
        fs::create_dir_all(&other).expect("mkdir");
        assert!(!is_dirty(&paths, &other).expect("read"));
    }

    #[test]
    fn verbatim_prefix_is_stripped() {
        assert_eq!(strip_verbatim(r"\\?\C:\Games\FF"), r"C:\Games\FF");
        assert_eq!(strip_verbatim(r"\\?\UNC\nas\share\FF"), r"\\nas\share\FF");
        assert_eq!(strip_verbatim(r"C:\Games"), r"C:\Games");
    }

    #[test]
    fn corrupt_marker_file_is_an_error_not_clean() {
        let (_dir, paths, install) = setup();
        let file = paths.patch_dirty_file();
        fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        fs::write(&file, b"{not json").expect("write");
        assert!(is_dirty(&paths, &install).is_err());
        assert!(ensure_clean(&paths, Region::Global, &install).is_err());
    }
}
