//! 衛月：兩來源的下載安裝、runtime、assets、profile、Injector launch（protocols §1.5、§2.5）。
//!
//! `prepare`、`remove_installed` 會動到 `staging/` 與正式目錄，只能在 app-wide mutex 內呼叫。

mod assets;
mod detect;
mod dll_version;
mod goatcorp;
mod install;
mod profile;
mod runner;
mod runtime;
mod yanmucorp;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::error::{AppError, AppResult};
use crate::store::{AppPaths, DalamudChannel, Profile, YanmucorpVersion};

pub use detect::{DetectedDalamudData, detect_from_env};
pub use dll_version::file_version;
pub use install::{CancelToken, DownloadCancel, Progress, Staging, download_to_file, download_to_file_exact, fetch_bytes, http_client, ProgressFn, parse_json, safe_relative_path};
pub use profile::{
    GameProcess, account_processes, discard_profile_dir, duplicate_profile_data, ensure_profile_paths, is_alive,
    mark_account_running, mark_running, profile_data_dir, profile_in_use, profile_log_dir, running_account_pids,
    running_accounts, unmark_account_running, unmark_running, wait_for_exit,
};
#[cfg(test)]
pub use profile::process_identity;
pub use runner::{InjectMode, InjectRequest, PluginLoad, launch_plain, launch_with_dalamud, watch_early_exit};
pub use yanmucorp::{ReleaseInfo, list_releases};

/// 本體來源目錄名（`dalamud/<source>/<version>/`）。
const SOURCES: [&str; 2] = [goatcorp::SOURCE, yanmucorp::SOURCE];

/// 注入所需的三個目錄都已核對並提交。
#[derive(Debug, Clone)]
pub struct PreparedDalamud {
    pub injector_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub assets_dir: PathBuf,
    /// goatcorp 為 AssemblyVersion，yanmucorp 為 release tag。
    pub version: String,
    pub inject_mode: InjectMode,
}

/// 依版本選擇確保本體、runtime、assets 就緒。goatcorp 另需本機遊戲版本與
/// `SupportedGameVer` 相符（`install_dir`＝該區服遊戲安裝根目錄）。被 `cancel` 取消時回 `Cancelled`。
pub async fn prepare(
    paths: &AppPaths,
    channel: &DalamudChannel,
    install_dir: &Path,
    progress: &install::ProgressFn,
    cancel: &CancelToken,
) -> AppResult<PreparedDalamud> {
    install::purge_stale(paths);
    let client = install::http_client()?;
    match channel {
        DalamudChannel::Goatcorp => {
            let info = cancel.race(goatcorp::fetch_version_info(&client)).await?;
            goatcorp::check_game_version(&info, install_dir)?;
            let injector_dir =
                goatcorp::ensure_body(&client, paths, &info, progress, cancel).await?;
            // RuntimeRequired 為假時與 XL 相同不下載 runtime；目錄可能不存在。
            let runtime_dir = if info.runtime_required {
                runtime::ensure_goatcorp(&client, paths, &info.runtime_version, progress, cancel)
                    .await?
            } else {
                paths.runtime_dir(&info.runtime_version)
            };
            let assets_dir = assets::ensure(
                &client,
                paths,
                goatcorp::SOURCE,
                goatcorp::ASSET_META_URL,
                progress,
                cancel,
            )
            .await?;
            Ok(PreparedDalamud {
                injector_dir,
                runtime_dir,
                assets_dir,
                version: info.assembly_version,
                inject_mode: InjectMode::Launch,
            })
        }
        DalamudChannel::Yanmucorp { version } => {
            let tag = cancel.race(yanmucorp::resolve_tag(&client, version)).await?;
            let injector_dir = yanmucorp::ensure_body(&client, paths, &tag, progress, cancel).await?;
            let runtime_dir = runtime::ensure_nuget(
                &client,
                paths,
                yanmucorp::RUNTIME_VERSION,
                progress,
                cancel,
            )
            .await?;
            let assets_dir = assets::ensure(
                &client,
                paths,
                yanmucorp::SOURCE,
                yanmucorp::ASSET_META_URL,
                progress,
                cancel,
            )
            .await?;
            Ok(PreparedDalamud {
                injector_dir,
                runtime_dir,
                assets_dir,
                version: tag,
                // launch 模式下 yanmucorp Injector 會跑陸服專用的 FfxivArgLauncher.ArgFixer，在台服
                // 客戶端找不到修補目標（Nops Num:-1）而殺掉遊戲；改由本程式起遊戲後 attach（§2.5 備案）。
                inject_mode: InjectMode::Attach,
            })
        }
    }
}

/// 「剛準備好」的上限：登入時前端先呼叫 `install_dalamud`，再（國際服手動 OTP 時）問 OTP，接著才 `launch_account`。
const REUSE_WINDOW: Duration = Duration::from_secs(120);

/// 登入前先準備好的衛月（`install_dalamud` 的結果），讓緊接著的 `launch_account` 沿用一次，同一次登入不準備兩次。
/// 前端先呼叫 `install_dalamud` 是為了在問 OTP、登入之前做完可能要好幾分鐘的下載（OTP、台服 token、國際服 UID
/// 會在下載期間失效，spec B9-dalamud-before-login）。
#[derive(Default)]
pub struct PreparedCache(Mutex<Option<CachedPrepare>>);

struct CachedPrepare {
    channel: DalamudChannel,
    install_dir: PathBuf,
    prepared: PreparedDalamud,
    /// 準備時 runtime 目錄在不在：goatcorp `RuntimeRequired` 為假時不下載，目錄可能本來就不存在。
    runtime_present: bool,
    at: Instant,
}

impl PreparedCache {
    /// 記下剛準備好的結果，取代前一筆。
    pub fn store(&self, channel: &DalamudChannel, install_dir: &Path, prepared: &PreparedDalamud) {
        *self.lock() = Some(CachedPrepare {
            channel: channel.clone(),
            install_dir: install_dir.to_path_buf(),
            prepared: prepared.clone(),
            runtime_present: prepared.runtime_dir.is_dir(),
            at: Instant::now(),
        });
    }

    /// 有可沿用的結果就用它，不 await `prepare`；沒有才 await `prepare` 完整準備（線上取版本與 assets 清單、逐檔核對，
    /// 核對失敗就重新下載）。可沿用＝同通道、同遊戲安裝、[`REUSE_WINDOW`] 內準備好，本體依目錄內 `hashes.json`
    /// 重核通過（同完整準備對已安裝本體的核對），assets 目錄與準備時就在的 runtime 目錄仍在。不論沿用與否，
    /// 暫存的結果都只用一次。
    pub async fn reuse_or_prepare(
        &self,
        channel: &DalamudChannel,
        install_dir: &Path,
        prepare: impl Future<Output = AppResult<PreparedDalamud>>,
    ) -> AppResult<PreparedDalamud> {
        if let Some(prepared) = self.take(channel, install_dir).await {
            return Ok(prepared);
        }
        prepare.await
    }

    async fn take(&self, channel: &DalamudChannel, install_dir: &Path) -> Option<PreparedDalamud> {
        let cached = self.lock().take()?;
        if cached.channel != *channel || cached.install_dir != install_dir || cached.at.elapsed() > REUSE_WINDOW {
            return None;
        }
        let prepared = cached.prepared;
        let dirs_present =
            prepared.assets_dir.is_dir() && (!cached.runtime_present || prepared.runtime_dir.is_dir());
        // 重核不符或讀不到都不沿用：完整準備會再核對一次，不符就重新下載，讀不到就回報錯誤。
        let body_ok = matches!(install::committed_body_ok(&prepared.injector_dir).await, Ok(true));
        (dirs_present && body_ok).then_some(prepared)
    }

    fn lock(&self) -> MutexGuard<'_, Option<CachedPrepare>> {
        // 臨界區只換掉一個值，panic 不會留下半更新的狀態，poison 後可直接沿用。
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// 已安裝的 Dalamud 本體版本。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledVersion {
    pub source: String,
    /// 安裝目錄名（goatcorp 版本號或 yanmucorp release tag）。
    pub version: String,
    /// 該目錄下 `Dalamud.dll` 的 FileVersion；讀不到為 `None`。
    pub dalamud_version: Option<String>,
}

/// 列出 `dalamud/<source>/<version>/`，依來源、版本排序。
pub fn list_installed(paths: &AppPaths) -> AppResult<Vec<InstalledVersion>> {
    let mut installed = Vec::new();
    for source in SOURCES {
        // 與 AppPaths::dalamud_dir 同一配置。
        let dir = paths.root().join("dalamud").join(source);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(AppError::io("讀取 Dalamud 目錄", e)),
        };
        for entry in entries {
            let entry = entry.map_err(|e| AppError::io("讀取 Dalamud 目錄", e))?;
            let is_dir = entry
                .file_type()
                .map_err(|e| AppError::io("讀取 Dalamud 目錄", e))?
                .is_dir();
            // 只列本程式提交得出來的名稱，列出的項目都能交給 remove_installed。
            if let Ok(version) = entry.file_name().into_string()
                && is_dir
                && install::is_safe_component(&version)
            {
                let dalamud_version =
                    dll_version::file_version(&entry.path().join(install::DALAMUD_DLL));
                installed.push(InstalledVersion {
                    source: source.to_string(),
                    version,
                    dalamud_version,
                });
            }
        }
    }
    installed.sort_by(|a, b| (&a.source, &a.version).cmp(&(&b.source, &b.version)));
    Ok(installed)
}

/// 設定分頁 profile 卡片的資訊。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileInfo {
    pub id: String,
    pub plugin_count: usize,
    pub installed_version: Option<String>,
    /// `installed_version` 那個安裝目錄的 `Dalamud.dll` FileVersion。
    pub installed_dalamud_version: Option<String>,
}

/// `installed` 為 `list_installed` 的結果，讓多個 profile 共用一次目錄掃描。
/// id 不合法或插件目錄讀不到時插件數為 0：只是卡片顯示，不影響設定載入。
pub fn profile_info(paths: &AppPaths, profile: &Profile, installed: &[InstalledVersion]) -> ProfileInfo {
    let plugin_count = profile_data_dir(paths, profile)
        .ok()
        .and_then(|dir| detect::count_subdirs(&dir.join("installedPlugins")))
        .unwrap_or(0);
    let found = installed_version(&profile.channel, installed);
    ProfileInfo {
        id: profile.id.clone(),
        plugin_count,
        installed_version: found.map(|v| v.version.clone()),
        installed_dalamud_version: found.and_then(|v| v.dalamud_version.clone()),
    }
}

/// 依已安裝本體推算 profile 啟動時會用的版本（不連網）。goatcorp 取數字段最高者；yanmucorp
/// Pinned 只認該 tag，Latest 取字串最大的 tag（`yy-mm-dd-nn` 可字串排序）。
fn installed_version<'a>(
    channel: &DalamudChannel,
    installed: &'a [InstalledVersion],
) -> Option<&'a InstalledVersion> {
    let source = match channel {
        DalamudChannel::Goatcorp => goatcorp::SOURCE,
        DalamudChannel::Yanmucorp { .. } => yanmucorp::SOURCE,
    };
    let mut versions = installed.iter().filter(|v| v.source == source);
    match channel {
        DalamudChannel::Goatcorp => versions
            .filter_map(|v| numeric_segments(&v.version).map(|key| (key, v)))
            .max_by(|a, b| a.0.cmp(&b.0))
            .map(|(_, v)| v),
        DalamudChannel::Yanmucorp {
            version: YanmucorpVersion::Pinned { tag },
        } => versions.find(|v| &v.version == tag),
        DalamudChannel::Yanmucorp {
            version: YanmucorpVersion::Latest,
        } => versions.max_by(|a, b| a.version.cmp(&b.version)),
    }
}

/// `15.0.10.0` → `[15, 0, 10, 0]`；任一段不是數字回 `None`（不是本程式提交的版本目錄）。
fn numeric_segments(version: &str) -> Option<Vec<u64>> {
    version.split('.').map(|s| s.parse().ok()).collect()
}

/// 刪除一個已安裝版本。先整個移開：被遊戲佔用時 rename 失敗並回錯誤，正式目錄保持完整。
pub fn remove_installed(paths: &AppPaths, source: &str, version: &str) -> AppResult<()> {
    let target = paths.dalamud_dir(source, version);
    if !SOURCES.contains(&source) || !install::is_safe_component(version) || !target.is_dir() {
        return Err(AppError::NotFound {
            what: format!("Dalamud {source} {version}"),
        });
    }
    let aside = install::move_aside(paths, &target)?;
    fs::remove_dir_all(&aside).map_err(|e| AppError::io("刪除 Dalamud 檔案", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(items: &[(&str, &str)]) -> Vec<InstalledVersion> {
        items
            .iter()
            .map(|(source, version)| InstalledVersion {
                source: (*source).into(),
                version: (*version).into(),
                dalamud_version: None,
            })
            .collect()
    }

    fn found_version<'a>(channel: &DalamudChannel, list: &'a [InstalledVersion]) -> Option<&'a str> {
        installed_version(channel, list).map(|v| v.version.as_str())
    }

    fn yanmucorp(version: YanmucorpVersion) -> DalamudChannel {
        DalamudChannel::Yanmucorp { version }
    }

    #[test]
    fn installed_version_follows_channel_rules() {
        let list = installed(&[
            ("goatcorp", "15.0.3.6"),
            ("goatcorp", "15.0.10.0"),
            ("goatcorp", "9.9.9.9"),
            ("goatcorp", "not-a-version"),
            ("yanmucorp", "26-09-28-01"),
            ("yanmucorp", "26-10-03-01"),
            ("yanmucorp", "26-10-03-02"),
        ]);

        // 依數字段比較，不是字串比較（字串比較會選 9.9.9.9 或 15.0.3.6）。
        assert_eq!(
            found_version(&DalamudChannel::Goatcorp, &list),
            Some("15.0.10.0")
        );
        assert_eq!(
            found_version(&yanmucorp(YanmucorpVersion::Latest), &list),
            Some("26-10-03-02")
        );
        assert_eq!(
            found_version(
                &yanmucorp(YanmucorpVersion::Pinned {
                    tag: "26-09-28-01".into()
                }),
                &list
            ),
            Some("26-09-28-01")
        );
        assert_eq!(
            found_version(
                &yanmucorp(YanmucorpVersion::Pinned {
                    tag: "26-08-01-01".into()
                }),
                &list
            ),
            None
        );
        // 另一來源的版本不算。
        let only_goat = installed(&[("goatcorp", "15.0.10.0")]);
        assert_eq!(
            found_version(&yanmucorp(YanmucorpVersion::Latest), &only_goat),
            None
        );
        assert_eq!(found_version(&DalamudChannel::Goatcorp, &[]), None);
    }

    #[test]
    fn installed_version_keeps_dalamud_version_of_chosen_dir() {
        let mut list = installed(&[("yanmucorp", "26-09-28-01"), ("yanmucorp", "26-10-03-01")]);
        list[0].dalamud_version = Some("13.0.0.15".into());
        list[1].dalamud_version = Some("13.0.0.16".into());
        let found = installed_version(&yanmucorp(YanmucorpVersion::Latest), &list).unwrap();
        assert_eq!(found.version, "26-10-03-01");
        assert_eq!(found.dalamud_version.as_deref(), Some("13.0.0.16"));
    }

    // ── 單次準備（PreparedCache）──────────────────────────────────────────────

    const INJECTOR_BYTES: &[u8] = b"injector";

    /// 已提交的本體（Injector＋列出它的 hashes.json）、runtime 與 assets 目錄。
    fn prepared_fixture(root: &Path) -> PreparedDalamud {
        use md5::{Digest, Md5};
        let paths = AppPaths::new(root.join("app"));
        let injector_dir = paths.dalamud_dir(yanmucorp::SOURCE, "26-10-03-01");
        fs::create_dir_all(&injector_dir).expect("mkdir body");
        fs::write(injector_dir.join(install::INJECTOR_EXE), INJECTOR_BYTES).expect("injector");
        let table = install::HashTable::from([(
            install::INJECTOR_EXE.to_string(),
            hex::encode_upper(Md5::digest(INJECTOR_BYTES)),
        )]);
        fs::write(
            injector_dir.join(install::HASHES_FILE),
            serde_json::to_vec(&table).expect("json"),
        )
        .expect("hashes");
        let runtime_dir = paths.runtime_dir(yanmucorp::RUNTIME_VERSION);
        let assets_dir = paths.assets_dir(yanmucorp::SOURCE, "2");
        fs::create_dir_all(&runtime_dir).expect("mkdir runtime");
        fs::create_dir_all(&assets_dir).expect("mkdir assets");
        PreparedDalamud {
            injector_dir,
            runtime_dir,
            assets_dir,
            version: "26-10-03-01".into(),
            inject_mode: InjectMode::Attach,
        }
    }

    /// `reuse_or_prepare` 是否走了完整準備（替身回傳 `fallback`）。
    async fn prepared_fully(
        cache: &PreparedCache,
        channel: &DalamudChannel,
        install_dir: &Path,
        fallback: &PreparedDalamud,
    ) -> bool {
        let called = std::cell::Cell::new(false);
        cache
            .reuse_or_prepare(channel, install_dir, async {
                called.set(true);
                Ok(fallback.clone())
            })
            .await
            .expect("prepare");
        called.get()
    }

    #[tokio::test]
    async fn preparation_just_before_login_is_reused_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let prepared = prepared_fixture(dir.path());
        let install_dir = dir.path().join("FFXIV TC");
        let channel = yanmucorp(YanmucorpVersion::Latest);
        let cache = PreparedCache::default();

        cache.store(&channel, &install_dir, &prepared);
        let reused = cache
            .reuse_or_prepare(&channel, &install_dir, async { panic!("已準備好時不應再完整準備") })
            .await
            .expect("reuse");
        assert_eq!(reused.injector_dir, prepared.injector_dir);
        assert_eq!(reused.version, prepared.version);

        assert!(prepared_fully(&cache, &channel, &install_dir, &prepared).await, "暫存只用一次");
    }

    #[tokio::test]
    async fn unprepared_mismatched_stale_or_failed_check_prepares_fully() {
        let dir = tempfile::tempdir().expect("tempdir");
        let prepared = prepared_fixture(dir.path());
        let install_dir = dir.path().join("FFXIV TC");
        let channel = yanmucorp(YanmucorpVersion::Latest);
        let cache = PreparedCache::default();

        assert!(prepared_fully(&cache, &channel, &install_dir, &prepared).await, "沒有先準備");

        cache.store(&channel, &install_dir, &prepared);
        let pinned = yanmucorp(YanmucorpVersion::Pinned { tag: "26-09-28-01".into() });
        assert!(prepared_fully(&cache, &pinned, &install_dir, &prepared).await, "版本選擇不同");

        cache.store(&channel, &install_dir, &prepared);
        let other_install = dir.path().join("other");
        assert!(prepared_fully(&cache, &channel, &other_install, &prepared).await, "遊戲安裝不同");

        cache.store(&channel, &install_dir, &prepared);
        cache.lock().as_mut().expect("stored").at = Instant::now()
            .checked_sub(REUSE_WINDOW + Duration::from_secs(1))
            .expect("instant");
        assert!(prepared_fully(&cache, &channel, &install_dir, &prepared).await, "不是剛準備好的");

        // 本體重核不符：完整準備（核對不符就重新下載）。
        cache.store(&channel, &install_dir, &prepared);
        let injector = prepared.injector_dir.join(install::INJECTOR_EXE);
        fs::write(&injector, b"tampered").expect("tamper");
        assert!(prepared_fully(&cache, &channel, &install_dir, &prepared).await, "本體核對失敗");
        fs::write(&injector, INJECTOR_BYTES).expect("restore");

        cache.store(&channel, &install_dir, &prepared);
        fs::remove_dir(&prepared.assets_dir).expect("rm assets");
        assert!(prepared_fully(&cache, &channel, &install_dir, &prepared).await, "assets 目錄不見了");
    }
}
