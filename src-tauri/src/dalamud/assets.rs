//! 衛月 assets（protocols §1.5、§2.5），兩來源共用 → `assets/<source>/<version>/`。
//!
//! 格式相同、大小寫不同（2026-10-12 實測）：kamori Asset Meta 為 camelCase 且部分 `hash` 為 null；
//! yanmucorp `assetCN.json` 為 PascalCase 且沒有 `PackageUrl`。所以一律逐檔下載 `url`，
//! 有 hash 的以 SHA1 核對，沒有 hash 的只能要求檔案存在。

use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;

use super::install::{self, CancelToken, HashAlgo, ProgressFn, Staging};
use crate::error::{AppError, AppResult};
use crate::store::AppPaths;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssetMeta {
    #[serde(alias = "Version")]
    version: u64,
    #[serde(alias = "Assets")]
    assets: Vec<AssetEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssetEntry {
    #[serde(alias = "Url")]
    url: String,
    #[serde(alias = "FileName")]
    file_name: String,
    #[serde(alias = "Hash", default)]
    hash: Option<String>,
}

/// 上游清單已知過期的 hash：（檔名, 清單宣告值, 接受值）。只有檔名與宣告值都相符才替換，
/// 清單一更新就自動失效；鏡像檔案再變動則照常核對失敗。替換值是 2026-10-12 實測下載結果。
/// yanmucorp assetCN.json 的這個字型放在 USTC CTAN 鏡像，與清單不同源，hash 是真正的信任錨，
/// 所以釘死單一實測值，不放寬成「不符就接受」。
const KNOWN_DRIFT: &[(&str, &str, &str)] = &[(
    "UIRes/NotoSansCJKsc-Medium.otf",
    "C8AC9E680749BF31536971BC51DB257DDBAF3E68",
    "55A035F929EC089979A886AC98D92B3527B8FF38",
)];

fn accepted_hash<'a>(file_name: &str, declared: &'a str) -> &'a str {
    KNOWN_DRIFT
        .iter()
        .find(|(name, stale, _)| *name == file_name && stale.eq_ignore_ascii_case(declared))
        .map_or(declared, |(_, _, actual)| actual)
}

impl AssetMeta {
    fn expected(&self) -> impl Iterator<Item = (&str, Option<&str>)> {
        self.assets.iter().map(|a| {
            let hash = a.hash.as_deref().map(|h| accepted_hash(&a.file_name, h));
            (a.file_name.as_str(), hash)
        })
    }
}

/// 確保 `meta_url` 指定版本的 assets 已安裝；已提交的版本依清單重核，不符才重裝。
pub async fn ensure(
    client: &reqwest::Client,
    paths: &AppPaths,
    source: &str,
    meta_url: &str,
    progress: &ProgressFn,
    cancel: &CancelToken,
) -> AppResult<PathBuf> {
    let meta: AssetMeta = cancel
        .race(install::fetch_json(client.get(meta_url), "Dalamud assets 清單"))
        .await?;
    // 檔名會變成本機路徑；空清單會讓核對無條件通過。
    if meta.assets.is_empty()
        || meta
            .assets
            .iter()
            .any(|a| install::safe_relative_path(&a.file_name).is_none())
    {
        return Err(AppError::protocol("Dalamud assets 清單"));
    }
    let meta = Arc::new(meta);
    let target = paths.assets_dir(source, &meta.version.to_string());

    if target.is_dir() {
        let (dir, list) = (target.clone(), Arc::clone(&meta));
        let complete = install::run_blocking("核對 Dalamud assets", move || {
            install::still_complete(install::verify_files(&dir, list.expected(), HashAlgo::Sha1))
        })
        .await?;
        if complete {
            return Ok(target);
        }
    }

    let staging = Staging::create(paths, "assets")?;
    let content = staging.path().join("content");
    let total = meta.assets.len();
    for (index, asset) in meta.assets.iter().enumerate() {
        let rel = install::safe_relative_path(&asset.file_name)
            .ok_or_else(|| AppError::protocol("Dalamud assets 清單"))?;
        let stage = format!("下載 Dalamud assets（{}/{total}）", index + 1);
        install::download_to_file(client, &asset.url, &content.join(rel), &stage, progress, cancel)
            .await?;
    }

    install::report(progress, "核對 Dalamud assets", 0, None);
    let paths_owned = paths.clone();
    let target_owned = target.clone();
    let cancel = cancel.clone();
    install::run_blocking("安裝 Dalamud assets", move || {
        cancel.check()?;
        install::verify_files(&content, meta.expected(), HashAlgo::Sha1)?;
        cancel.check()?;
        staging.commit(&paths_owned, &content, &target_owned)
    })
    .await?;
    Ok(target)
}
