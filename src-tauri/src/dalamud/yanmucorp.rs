//! yanmucorp 衛月（台服，protocols §2.5）：GitHub release 清單、指定 tag 的本體下載與核對。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use reqwest::header::ACCEPT;
use serde::{Deserialize, Serialize};

use super::goatcorp::GameVersionCheck;
use super::install::{self, CancelToken, HASHES_FILE, ProgressFn, Staging};
use crate::error::{AppError, AppResult};
use crate::game;
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
/// Dalamud 以遊戲版本命名的 signature 快取目錄（`{working directory}/cachedSigs/{遊戲版本}.json`）。
const CACHED_SIGS_DIR: &str = "cachedSigs";

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

/// 繁中服版本比對（protocols §2.5「版本比對」）：`Enforce` 時 `tag` 推定是比本機遊戲舊的版本用的就回
/// `DalamudOlderThanGame`；`Skip`（該次登入選「強制載入」、設定頁下載）不比對。yanmucorp 沒有支援版本資料，
/// 依據只有該 tag 目錄裡 Dalamud 自己寫的 `cachedSigs` 檔名與 tag 的發布日期。
pub fn check_game_version(paths: &AppPaths, tag: &str, install_dir: &Path, check: GameVersionCheck) -> AppResult<()> {
    match check {
        GameVersionCheck::Enforce => {}
        GameVersionCheck::Skip => return Ok(()),
    }
    let installed = game::game_version(install_dir)?;
    let cached = cached_sig_versions(&paths.dalamud_dir(SOURCE, tag).join(CACHED_SIGS_DIR))?;
    if older_than_game(tag, &installed, &cached) {
        return Err(AppError::DalamudOlderThanGame { tag: tag.to_string(), installed });
    }
    Ok(())
}

/// `cachedSigs` 裡以遊戲版本命名的快取檔（`{版本}.json`）的版本號。檔名不是 `數字(.數字)*.json` 的忽略
/// （例如 ClientStructs 的 `cs.json`）；目錄不存在＝這個 tag 還沒跑過，沒有紀錄。
fn cached_sig_versions(dir: &Path) -> AppResult<Vec<Vec<u64>>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(AppError::io("讀取 Dalamud 快取", e)),
    };
    let mut versions = Vec::new();
    for entry in entries {
        let name = entry.map_err(|e| AppError::io("讀取 Dalamud 快取", e))?.file_name();
        if let Some(version) = name.to_str().and_then(|n| n.strip_suffix(".json")).and_then(numeric_segments) {
            versions.push(version);
        }
    }
    Ok(versions)
}

/// 判定 `tag` 是不是比本機遊戲版本 `installed` 舊的版本用的（`cached`＝該 tag 跑過的遊戲版本）：
/// 跑過本機版本＝可用；只跑過較舊的版本＝舊；tag 發布日期早於遊戲版本的建置日（`ffxivgame.ver` 開頭的
/// `yyyy.mm.dd`）＝舊。其餘（沒跑過、日期不早於建置日、格式讀不出）推定可用。
fn older_than_game(tag: &str, installed: &str, cached: &[Vec<u64>]) -> bool {
    let Some(game) = numeric_segments(installed) else {
        return false;
    };
    if cached.contains(&game) {
        return false;
    }
    if cached.iter().any(|v| *v < game) {
        return true;
    }
    match (tag_release_date(tag), game.get(..3)) {
        (Some(released), Some(&[year, month, day])) => released < (year, month, day),
        _ => false,
    }
}

/// `數字(.數字)*` 的各段數值；有任一段不是純數字就不是版本號。
fn numeric_segments(s: &str) -> Option<Vec<u64>> {
    s.split('.').map(parse_digits).collect()
}

/// tag `yy-mm-dd-nn` 的發布日期 `(yyyy, mm, dd)`；不是這個形式回 `None`。
fn tag_release_date(tag: &str) -> Option<(u64, u64, u64)> {
    let parts = tag.split('-').map(parse_digits).collect::<Option<Vec<_>>>()?;
    match parts[..] {
        [yy, mm, dd, _] => Some((2000 + yy, mm, dd)),
        _ => None,
    }
}

/// 只接受 ASCII 數字（`u64::from_str` 會接受開頭的 `+`）。
fn parse_digits(s: &str) -> Option<u64> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 以 `installed` 為本機遊戲版本、`cached` 為 `cachedSigs` 檔名（含副檔名），跑一次 `Enforce` 比對。
    fn enforce(tag: &str, installed: &str, cached: &[&str]) -> AppResult<()> {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().join("app"));
        let install_dir = dir.path().join("FFXIV TC");
        fs::create_dir_all(install_dir.join("game")).expect("mkdir game");
        fs::write(install_dir.join("game/ffxivgame.ver"), installed).expect("ver");
        if !cached.is_empty() {
            let sigs = paths.dalamud_dir(SOURCE, tag).join(CACHED_SIGS_DIR);
            fs::create_dir_all(&sigs).expect("mkdir sigs");
            for name in cached {
                fs::write(sigs.join(name), "{}").expect("sig");
            }
        }
        check_game_version(&paths, tag, &install_dir, GameVersionCheck::Enforce)
    }

    fn is_older(result: AppResult<()>, tag: &str, version: &str) -> bool {
        matches!(result, Err(AppError::DalamudOlderThanGame { tag: ref t, ref installed }) if t == tag && installed == version)
    }

    /// 2026-11-10 本機：`26-10-03-01` 已在 7.25（`2026.09.14`）跑過，還有不是版本號的 `cs.json`。
    #[test]
    fn tag_that_ran_on_the_installed_game_passes() {
        assert!(enforce("26-10-03-01", "2026.09.14.0000.0000", &["2026.09.14.0000.0000.json", "cs.json"]).is_ok());
    }

    /// 遊戲改版後舊 tag 還沒更新：新版建置日在 tag 發布日之後或之前，都因只跑過較舊版本而擋下。
    #[test]
    fn tag_that_only_ran_on_older_games_is_blocked() {
        let cached = ["2026.09.14.0000.0000.json", "cs.json"];
        for installed in ["2026.11.04.0000.0000", "2026.09.30.0000.0000"] {
            assert!(is_older(enforce("26-10-03-01", installed, &cached), "26-10-03-01", installed), "{installed}");
        }
    }

    /// 沒跑過的 tag：發布日早於遊戲建置日就擋；不早於（上游為新版發布的 tag）就放行，Latest 因此能自動換上。
    #[test]
    fn unrun_tag_is_judged_by_release_date() {
        let installed = "2026.11.04.0000.0000";
        assert!(is_older(enforce("26-10-03-01", installed, &[]), "26-10-03-01", installed));
        assert!(enforce("26-11-04-01", installed, &[]).is_ok());
        assert!(enforce("26-11-18-01", installed, &["cs.json"]).is_ok());
    }

    /// tag 或遊戲版本讀不出日期／版本時不推定，照常注入。
    #[test]
    fn unparseable_tag_or_version_is_not_blocked() {
        assert!(enforce("nightly", "2026.11.04.0000.0000", &[]).is_ok());
        assert!(enforce("26-10-03", "2026.11.04.0000.0000", &[]).is_ok());
        assert!(enforce("26-10-03-01", "2026.11.04.beta", &["2026.09.14.0000.0000.json"]).is_ok());
    }

    /// 強制載入（`Skip`）不比對。
    #[test]
    fn skip_does_not_check() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().join("app"));
        let install_dir = dir.path().join("FFXIV TC");
        fs::create_dir_all(install_dir.join("game")).expect("mkdir game");
        fs::write(install_dir.join("game/ffxivgame.ver"), "2026.11.04.0000.0000").expect("ver");
        assert!(check_game_version(&paths, "26-10-03-01", &install_dir, GameVersionCheck::Skip).is_ok());
    }

    #[test]
    fn version_segments_are_strictly_numeric() {
        assert_eq!(numeric_segments("2026.09.14.0000.0000"), Some(vec![2026, 9, 14, 0, 0]));
        assert_eq!(numeric_segments("cs"), None);
        assert_eq!(numeric_segments("2026..14"), None);
        assert_eq!(numeric_segments("+2026.09"), None);
        assert_eq!(tag_release_date("26-10-03-01"), Some((2026, 10, 3)));
        assert_eq!(tag_release_date("26-10-03-01-2"), None);
    }
}
