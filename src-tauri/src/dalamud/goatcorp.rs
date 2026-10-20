//! goatcorp 衛月（國際服，protocols §1.5）：版本資訊、本體下載與核對。
//! goatcorp 只提供當前 release，不能鎖舊版。

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::install::{self, CancelToken, ProgressFn, Staging};
use crate::error::{AppError, AppResult};
use crate::game;
use crate::store::AppPaths;

pub const SOURCE: &str = "goatcorp";
const VERSION_INFO_URL: &str =
    "https://kamori.goats.dev/Dalamud/Release/VersionInfo?track=release&bucket=Control";
pub const ASSET_META_URL: &str = "https://kamori.goats.dev/Dalamud/Asset/Meta";

/// kamori 實際回 camelCase（2026-10-12 實測）；alias 保留 XL 型別的 PascalCase 以防伺服器改回。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionInfo {
    #[serde(alias = "AssemblyVersion")]
    pub assembly_version: String,
    #[serde(alias = "SupportedGameVer")]
    pub supported_game_ver: String,
    #[serde(alias = "RuntimeVersion")]
    pub runtime_version: String,
    #[serde(alias = "RuntimeRequired")]
    pub runtime_required: bool,
    #[serde(alias = "DownloadUrl")]
    pub download_url: String,
}

pub async fn fetch_version_info(client: &reqwest::Client) -> AppResult<VersionInfo> {
    let info: VersionInfo =
        install::fetch_json(client.get(VERSION_INFO_URL), "國際服 Dalamud 版本資訊").await?;
    // 兩個版本字串都會變成目錄名稱。
    if !install::is_safe_component(&info.assembly_version)
        || !install::is_safe_component(&info.runtime_version)
    {
        return Err(AppError::protocol("國際服 Dalamud 版本資訊"));
    }
    Ok(info)
}

/// 本次登入是否比對 `SupportedGameVer`（protocols §1.5）。預設比對；`Skip` 只在使用者該次登入選「強制載入」時送來。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GameVersionCheck {
    #[default]
    Enforce,
    Skip,
}

/// `Enforce` 時 `SupportedGameVer` 必須等於本機 `ffxivgame.ver`，否則不可注入；`Skip` 不比對。
pub fn check_game_version(info: &VersionInfo, install_dir: &Path, check: GameVersionCheck) -> AppResult<()> {
    match check {
        GameVersionCheck::Enforce => {}
        GameVersionCheck::Skip => return Ok(()),
    }
    let installed = game::game_version(install_dir)?;
    if info.supported_game_ver != installed {
        return Err(AppError::DalamudGameVersionMismatch {
            supported: info.supported_game_ver.clone(),
            installed,
        });
    }
    Ok(())
}

/// 確保本體在 `dalamud/goatcorp/<AssemblyVersion>/`；已提交且重核通過就不覆寫。
pub async fn ensure_body(
    client: &reqwest::Client,
    paths: &AppPaths,
    info: &VersionInfo,
    progress: &ProgressFn,
    cancel: &CancelToken,
) -> AppResult<PathBuf> {
    let target = paths.dalamud_dir(SOURCE, &info.assembly_version);
    if install::committed_body_ok(&target).await? {
        return Ok(target);
    }

    let staging = Staging::create(paths, SOURCE)?;
    let archive = staging.path().join("dalamud.zip");
    install::download_to_file(client, &info.download_url, &archive, "下載 Dalamud", progress, cancel)
        .await?;

    install::report(progress, "解壓並核對 Dalamud", 0, None);
    let content = staging.path().join("content");
    let paths_owned = paths.clone();
    let target_owned = target.clone();
    let cancel = cancel.clone();
    install::run_blocking("安裝 Dalamud", move || {
        cancel.check()?;
        install::extract_zip(&archive, &content)?;
        cancel.check()?;
        install::commit_dalamud_body(&paths_owned, staging, &content, &target_owned, &cancel)
    })
    .await?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn info(supported: &str) -> VersionInfo {
        VersionInfo {
            assembly_version: "15.0.3.8".into(),
            supported_game_ver: supported.into(),
            runtime_version: "10.0.0".into(),
            runtime_required: true,
            download_url: "https://example.invalid/dalamud.zip".into(),
        }
    }

    #[test]
    fn game_version_mismatch_blocks_unless_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("game")).expect("mkdir");
        fs::write(dir.path().join("game/ffxivgame.ver"), "2026.10.16.0000.0000\r\n").expect("write");
        let stale = info("2026.10.06.0000.0000");

        let err = check_game_version(&stale, dir.path(), GameVersionCheck::Enforce).expect_err("mismatch");
        assert!(matches!(
            err,
            AppError::DalamudGameVersionMismatch { ref supported, ref installed }
                if supported == "2026.10.06.0000.0000" && installed == "2026.10.16.0000.0000"
        ));
        assert!(check_game_version(&stale, dir.path(), GameVersionCheck::Skip).is_ok());
        assert!(check_game_version(&info("2026.10.16.0000.0000"), dir.path(), GameVersionCheck::Enforce).is_ok());
    }
}
