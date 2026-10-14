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

/// `SupportedGameVer` 必須等於本機 `ffxivgame.ver`，否則不可注入。
pub fn check_game_version(info: &VersionInfo, install_dir: &Path) -> AppResult<()> {
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
