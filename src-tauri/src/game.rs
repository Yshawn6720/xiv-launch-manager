//! 遊戲安裝路徑偵測、版本檔讀取（protocols §1.1、§2.3、§2.4）與遊戲設定資料夾（`文件\My Games\` 下）。

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};
use crate::region::Region;

/// 遊戲本體不存在時視為的版本（XL `Constants.cs` BASE_GAME_VERSION）。
pub const BASE_GAME_VERSION: &str = "2012.01.01.0000.0000";
/// `game/sqpack/ex{N}` 的上限（目前到 7.x 為 ex5）。
pub const MAX_EXPANSION: u8 = 5;

pub fn game_exe(install: &Path) -> PathBuf {
    install.join("game").join("ffxiv_dx11.exe")
}

pub fn is_valid_install(install: &Path) -> bool {
    game_exe(install).is_file()
}

/// 依登錄檔找安裝路徑；找不到或路徑無效回傳 `None`。
pub fn detect_install(region: Region) -> Option<PathBuf> {
    let candidate = match region {
        Region::Global => detect_global(),
        Region::Tw => detect_tw(),
    }?;
    is_valid_install(&candidate).then_some(candidate)
}

fn detect_global() -> Option<PathBuf> {
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;
    // SE 安裝程式寫入的 InstallLocation 是 `...\SquareEnix`，遊戲在其下一層。
    const KEY: &str = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\{2B41E132-07DF-4925-A3D3-F2D1765CCDFE}";
    let key = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(KEY).ok()?;
    let location: String = key.get_value("InstallLocation").ok()?;
    let base = PathBuf::from(location);
    let nested = base.join("FINAL FANTASY XIV - A Realm Reborn");
    if is_valid_install(&nested) { Some(nested) } else { Some(base) }
}

fn detect_tw() -> Option<PathBuf> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Classes\com.userjoy.ffxiv")
        .ok()?;
    let dir: String = key.get_value("InstallDir").ok()?;
    Some(PathBuf::from(dir.trim_end_matches('\\')))
}

fn read_ver(file: &Path) -> AppResult<Option<String>> {
    match fs::read_to_string(file) {
        Ok(s) => Ok(Some(s.trim().to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(AppError::io(format!("讀取 {}", file.display()), e)),
    }
}

/// `game/ffxivgame.ver`；不存在時為 BASE_GAME_VERSION。
pub fn game_version(install: &Path) -> AppResult<String> {
    Ok(read_ver(&install.join("game").join("ffxivgame.ver"))?
        .unwrap_or_else(|| BASE_GAME_VERSION.to_string()))
}

/// `boot/ffxivboot.ver`；不存在時為 BASE_GAME_VERSION。
pub fn boot_version(install: &Path) -> AppResult<String> {
    Ok(read_ver(&install.join("boot").join("ffxivboot.ver"))?
        .unwrap_or_else(|| BASE_GAME_VERSION.to_string()))
}

/// `game/sqpack/ex{n}/ex{n}.ver`；不存在回傳 `None`。
pub fn expansion_version(install: &Path, n: u8) -> AppResult<Option<String>> {
    let name = format!("ex{n}");
    read_ver(
        &install
            .join("game")
            .join("sqpack")
            .join(&name)
            .join(format!("{name}.ver")),
    )
}

/// 額外遊戲參數（protocols §2.6）：空白分隔的 `key=value`，key、value 皆非空。
/// 錯誤只指出第幾項（從 1 起算），不帶原文。
pub fn parse_extra_args(text: &str) -> AppResult<Vec<(String, String)>> {
    text.split_whitespace()
        .enumerate()
        .map(|(i, token)| match token.split_once('=') {
            Some((key, value)) if !key.is_empty() && !value.is_empty() => {
                Ok((key.to_string(), value.to_string()))
            }
            _ => Err(AppError::InvalidExtraArgs { index: i + 1 }),
        })
        .collect()
}

/// 遊戲設定資料夾（`FFXIV.cfg`、角色設定等）在「文件」下的相對位置；兩服各自一個資料夾。
fn config_dir_in(documents: &Path, region: Region) -> PathBuf {
    documents.join("My Games").join(match region {
        Region::Global => "FINAL FANTASY XIV - A Realm Reborn",
        Region::Tw => "FINAL FANTASY XIV - TC",
    })
}

/// 本機的遊戲設定資料夾。只組路徑，不確認存在、不建立（遊戲第一次啟動時自己建）。
pub fn config_dir(region: Region) -> AppResult<PathBuf> {
    Ok(config_dir_in(&documents_dir()?, region))
}

/// Known Folder「文件」（`FOLDERID_Documents`）：文件被 OneDrive 或使用者重新導向時也是實際位置，
/// 不用 `%USERPROFILE%\Documents` 拼。
fn documents_dir() -> AppResult<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_Documents, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

    // SAFETY: rfid 指向靜態 GUID；htoken 為 None＝目前使用者。成功時回傳的字串由下方以 CoTaskMemFree 釋放。
    // quality-allow: Win32 FFI，SHGetKnownFolderPath 沒有安全包裝
    let raw = unsafe { SHGetKnownFolderPath(&FOLDERID_Documents, KF_FLAG_DEFAULT, None) }.map_err(|e| {
        AppError::Io {
            context: "取得「文件」資料夾".to_string(),
            message: e.message(),
        }
    })?;
    // SAFETY: raw 是 SHGetKnownFolderPath 成功回傳、以 0 結尾的 UTF-16 字串；先複製內容，再釋放且只釋放一次。
    // quality-allow: Win32 FFI，讀取與釋放 shell 配置的字串沒有安全包裝
    let path = unsafe {
        let path = OsString::from_wide(raw.as_wide());
        CoTaskMemFree(Some(raw.0.cast_const().cast()));
        path
    };
    Ok(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_is_per_region_under_my_games() {
        let documents = Path::new(r"D:\OneDrive\文件");
        assert_eq!(
            config_dir_in(documents, Region::Global),
            Path::new(r"D:\OneDrive\文件\My Games\FINAL FANTASY XIV - A Realm Reborn")
        );
        assert_eq!(
            config_dir_in(documents, Region::Tw),
            Path::new(r"D:\OneDrive\文件\My Games\FINAL FANTASY XIV - TC")
        );
    }

    #[test]
    fn extra_args_are_key_value_pairs() {
        assert_eq!(parse_extra_args("  ").expect("empty"), []);
        assert_eq!(
            parse_extra_args(" SYS.ScreenMode=2\tDEV.X=a=b ").expect("pairs"),
            [
                ("SYS.ScreenMode".to_string(), "2".to_string()),
                ("DEV.X".to_string(), "a=b".to_string()),
            ]
        );
        for (text, index) in [("a=1 b", 2), ("=1", 1), ("a= b=2", 1)] {
            assert!(
                matches!(parse_extra_args(text), Err(AppError::InvalidExtraArgs { index: i }) if i == index),
                "{text}"
            );
        }
    }

    #[test]
    fn missing_version_files_fall_back_to_base_or_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(game_version(dir.path()).expect("game"), BASE_GAME_VERSION);
        assert_eq!(expansion_version(dir.path(), 1).expect("ex1"), None);

        let ex1 = dir.path().join("game/sqpack/ex1");
        fs::create_dir_all(&ex1).expect("mkdir");
        fs::write(ex1.join("ex1.ver"), "2026.09.10.0000.0000\r\n").expect("write");
        assert_eq!(
            expansion_version(dir.path(), 1).expect("ex1").as_deref(),
            Some("2026.09.10.0000.0000")
        );
    }
}
