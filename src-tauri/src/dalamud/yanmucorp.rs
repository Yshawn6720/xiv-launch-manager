//! yanmucorp 衛月（台服，protocols §2.5）：GitHub release 清單、指定 tag 的本體下載與核對。

use std::fs;
use std::path::PathBuf;

use reqwest::header::ACCEPT;
use serde::{Deserialize, Serialize};

use super::install::{self, CancelToken, HASHES_FILE, ProgressFn, Staging};
use crate::error::{AppError, AppResult};
use crate::region::Region;
use crate::store::{AppPaths, YanmucorpVersion};

pub const SOURCE: &str = "yanmucorp";
/// yanmucorp 的 runtime 固定 .NET 9.0.11（TC 實測可用的版本；上游 API 會給過新的版本）。
pub const RUNTIME_VERSION: &str = "9.0.11";
pub const ASSET_META_URL: &str =
    "https://raw.githubusercontent.com/yanmucorp/DalamudAssets/master/assetCN.json";

const RELEASES_URL: &str = "https://api.github.com/repos/yanmucorp/Dalamud/releases?per_page=100";
const DOWNLOAD_BASE: &str = "https://github.com/yanmucorp/Dalamud/releases/download";
const BODY_ARCHIVE: &str = "latest.7z";

/// 可安裝的 release（給介面選 tag）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseInfo {
    pub tag: String,
    pub published_at: String,
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    published_at: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
}

impl GitHubRelease {
    fn has_asset(&self, name: &str) -> bool {
        self.assets.iter().any(|a| a.name == name)
    }
}

/// 列出可安裝的 release，新到舊。
pub async fn list_releases() -> AppResult<Vec<ReleaseInfo>> {
    let client = install::http_client()?;
    fetch_releases(&client).await
}

async fn fetch_releases(client: &reqwest::Client) -> AppResult<Vec<ReleaseInfo>> {
    let releases: Vec<GitHubRelease> = install::fetch_json(
        client
            .get(RELEASES_URL)
            .header(ACCEPT, "application/vnd.github+json"),
        "繁中服 Dalamud 版本清單",
    )
    .await?;
    // 只留能完整核對的正式 release：tag 會變成目錄名稱，且必須同時有本體與 hash 表。
    let mut list: Vec<ReleaseInfo> = releases
        .into_iter()
        .filter(|r| {
            !r.draft
                && !r.prerelease
                && install::is_safe_component(&r.tag_name)
                && r.has_asset(BODY_ARCHIVE)
                && r.has_asset(HASHES_FILE)
        })
        .filter_map(|r| {
            Some(ReleaseInfo {
                published_at: r.published_at?,
                tag: r.tag_name,
            })
        })
        .collect();
    // GitHub 的 published_at 是固定格式的 UTC ISO 8601，字串序即時間序。
    list.sort_by(|a, b| b.published_at.cmp(&a.published_at));
    Ok(list)
}

/// `Latest` 解析為最新可安裝 release 的 tag；`Pinned` 直接用（可離線回滾到已安裝的 tag）。
pub async fn resolve_tag(client: &reqwest::Client, version: &YanmucorpVersion) -> AppResult<String> {
    match version {
        YanmucorpVersion::Latest => fetch_releases(client)
            .await?
            .into_iter()
            .next()
            .map(|r| r.tag)
            .ok_or_else(|| AppError::NotFound {
                what: format!("可安裝的{} Dalamud 版本", Region::Tw),
            }),
        YanmucorpVersion::Pinned { tag } => {
            if install::is_safe_component(tag) {
                Ok(tag.clone())
            } else {
                Err(AppError::NotFound {
                    what: format!("{} Dalamud 版本「{tag}」", Region::Tw),
                })
            }
        }
    }
}

/// 確保本體在 `dalamud/yanmucorp/<tag>/`；已提交且重核通過就不覆寫。
/// 核對以同一 release 的 `hashes.json` asset 為準，並寫進安裝目錄供之後重核。
pub async fn ensure_body(
    client: &reqwest::Client,
    paths: &AppPaths,
    tag: &str,
    progress: &ProgressFn,
    cancel: &CancelToken,
) -> AppResult<PathBuf> {
    let target = paths.dalamud_dir(SOURCE, tag);
    if install::committed_body_ok(&target).await? {
        return Ok(target);
    }

    // hash 表很小，先取得並驗格式，不對就不必下載本體。
    let hashes = cancel
        .race(install::fetch_bytes(
            client.get(format!("{DOWNLOAD_BASE}/{tag}/{HASHES_FILE}")),
            "下載 Dalamud 核對表",
        ))
        .await?;
    install::parse_hash_table(&hashes, "Dalamud 核對表")?;

    let staging = Staging::create(paths, SOURCE)?;
    let archive = staging.path().join(BODY_ARCHIVE);
    install::download_to_file(
        client,
        &format!("{DOWNLOAD_BASE}/{tag}/{BODY_ARCHIVE}"),
        &archive,
        "下載 Dalamud",
        progress,
        cancel,
    )
    .await?;

    install::report(progress, "解壓並核對 Dalamud", 0, None);
    let content = staging.path().join("content");
    let paths_owned = paths.clone();
    let target_owned = target.clone();
    let cancel = cancel.clone();
    install::run_blocking("安裝 Dalamud", move || {
        cancel.check()?;
        install::extract_7z(&archive, &content)?;
        cancel.check()?;
        // 壓縮檔內也有 hashes.json；以 release asset 覆蓋，確保核對依據不是壓縮檔自己提供的。
        fs::write(content.join(HASHES_FILE), &hashes)
            .map_err(|e| AppError::io("寫入 hashes.json", e))?;
        install::commit_dalamud_body(&paths_owned, staging, &content, &target_owned, &cancel)
    })
    .await?;
    Ok(target)
}
