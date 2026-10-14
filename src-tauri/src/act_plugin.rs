//! ACT 的 FFXIV 插件版本（protocols §6.6）：各區服 ACT 已安裝的 FFXIV_ACT_Plugin 版本，與 GitHub 上
//! ravahn/FFXIV_ACT_Plugin 最新 release 比較。ACT 開啟時會自動更新插件，本程式只顯示與提醒，不下載、不改任何檔案。

use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

use reqwest::header::ACCEPT;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::act_bridge::{self, Env, FfxivPlugin};
use crate::dalamud;
use crate::error::{AppError, AppResult};
use crate::region::Region;
use crate::store::ActConfig;

const LATEST_URL: &str = "https://api.github.com/repos/ravahn/FFXIV_ACT_Plugin/releases/latest";
const LATEST_CONTEXT: &str = "FFXIV 插件最新版";
/// 兩次查詢最新版的最短間隔：未驗證的 GitHub API 每個 IP 每小時只有 60 次。
const LATEST_TTL: Duration = Duration::from_secs(60 * 60);
/// 單次查詢的總逾時：已安裝版本與最新版一起回給畫面，網路卡住時不讓畫面一直停在「檢查中」。
const LATEST_TIMEOUT: Duration = Duration::from_secs(10);

/// 四段數字的版本（FileVersion `a.b.c.d`、release tag `3.0.3.3`），依各段數值比較。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version([u32; 4]);

impl Version {
    /// 恰好四段、每段只有 ASCII 數字；其他格式（`v3.0.3.3`、三段、五段、空段、帶空白）都是 `None`。
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = [0u32; 4];
        let mut fields = text.split('.');
        for part in &mut parts {
            let field = fields.next()?;
            if field.is_empty() || !field.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            *part = field.parse().ok()?;
        }
        fields.next().is_none().then_some(Self(parts))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d] = self.0;
        write!(f, "{a}.{b}.{c}.{d}")
    }
}

/// 一個區服的 FFXIV 插件狀態。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum PluginStatus {
    /// 該區服沒有設定 ACT。
    NotConfigured,
    /// 設定的 ACT 執行檔不存在。
    ExeMissing,
    /// ACT 設定檔不存在：這個 ACT 還沒開過。
    NeverOpened,
    /// ACT 設定檔讀不到或讀不懂。
    Unreadable,
    /// ACT 沒有登記 FFXIV 插件，或登記的檔案不存在。
    NotInstalled,
    /// 插件檔存在，但讀不到四段數字的 FileVersion。
    VersionUnknown,
    /// `outdated`：最新版比已安裝的新；查不到最新版時為 false。
    Installed { version: String, outdated: bool },
}

/// 各區服 FFXIV 插件的狀態與最新版；兩服用同一個 ACT 時狀態相同。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginVersions {
    pub global: PluginStatus,
    pub tw: PluginStatus,
    /// GitHub 最新 release 的版本；查詢失敗為 `None`。
    pub latest: Option<String>,
}

/// 各區服的插件狀態：已設定的 ACT 去重後各讀一次（`act_bridge::configured`），沒設定的區服是 `NotConfigured`。
/// 只讀檔案，任一 ACT 讀不到只影響它自己的狀態。
pub fn versions(config: &ActConfig, env: &Env, latest: Option<Version>) -> PluginVersions {
    let mut global = PluginStatus::NotConfigured;
    let mut tw = PluginStatus::NotConfigured;
    for (exe, regions) in act_bridge::configured(config) {
        let status = plugin_status(act_bridge::ffxiv_plugin(&exe, env), latest);
        for region in regions {
            *match region {
                Region::Global => &mut global,
                Region::Tw => &mut tw,
            } = status.clone();
        }
    }
    PluginVersions {
        global,
        tw,
        latest: latest.map(|v| v.to_string()),
    }
}

fn plugin_status(plugin: FfxivPlugin, latest: Option<Version>) -> PluginStatus {
    match plugin {
        FfxivPlugin::ExeMissing => PluginStatus::ExeMissing,
        FfxivPlugin::NeverOpened => PluginStatus::NeverOpened,
        FfxivPlugin::Unreadable => PluginStatus::Unreadable,
        FfxivPlugin::NotInstalled => PluginStatus::NotInstalled,
        FfxivPlugin::Installed(dll) => match installed_version(&dll) {
            Some(version) => PluginStatus::Installed {
                version: version.to_string(),
                outdated: is_outdated(version, latest),
            },
            None => PluginStatus::VersionUnknown,
        },
    }
}

/// 插件檔版本資源的 FileVersion（`VS_FIXEDFILEINFO`）。
fn installed_version(dll: &Path) -> Option<Version> {
    Version::parse(&dalamud::file_version(dll)?)
}

/// 有比已安裝的新的最新版；查不到最新版時不算。
fn is_outdated(installed: Version, latest: Option<Version>) -> bool {
    latest.is_some_and(|latest| latest > installed)
}

/// 查 GitHub 最新 release 的版本（`tag_name`）。
pub async fn fetch_latest() -> AppResult<Version> {
    let client = dalamud::http_client()?;
    let body = dalamud::fetch_bytes(
        client
            .get(LATEST_URL)
            .header(ACCEPT, "application/vnd.github+json")
            .timeout(LATEST_TIMEOUT),
        LATEST_CONTEXT,
    )
    .await?;
    parse_latest(&body)
}

/// GitHub `releases/latest` 回應的 `tag_name`；不是四段數字就當成查詢失敗。
fn parse_latest(body: &[u8]) -> AppResult<Version> {
    #[derive(Deserialize)]
    struct Release {
        tag_name: String,
    }
    let release: Release = dalamud::parse_json(body, LATEST_CONTEXT)?;
    Version::parse(&release.tag_name).ok_or_else(|| AppError::protocol(LATEST_CONTEXT))
}

/// 最新版的記憶體快取：成功與失敗都記，距上次查詢未滿 [`LATEST_TTL`] 就沿用；`force`（「重新檢查」）時重查。
/// 查詢在鎖內進行：同時進來的查詢等前一個查完、沿用它的結果，不重複連網。
#[derive(Default)]
pub struct LatestCache(Mutex<Option<Checked>>);

struct Checked {
    at: Instant,
    latest: Option<Version>,
}

impl LatestCache {
    /// 最新版；查詢失敗為 `None`（錯誤只記 log，不顯示）。
    pub async fn get<F, Fut>(&self, force: bool, fetch: F) -> Option<Version>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = AppResult<Version>>,
    {
        self.get_at(Instant::now(), force, fetch).await
    }

    async fn get_at<F, Fut>(&self, now: Instant, force: bool, fetch: F) -> Option<Version>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = AppResult<Version>>,
    {
        let mut cached = self.0.lock().await;
        if let Some(checked) = cached.as_ref().filter(|c| !force && now.saturating_duration_since(c.at) < LATEST_TTL) {
            return checked.latest;
        }
        let latest = match fetch().await {
            Ok(version) => Some(version),
            Err(e) => {
                eprintln!("{LATEST_CONTEXT}查詢失敗：{e}");
                None
            }
        };
        *cached = Some(Checked { at: now, latest });
        latest
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::store::ActSettings;

    /// 2026-10-14 實際的 `GET /repos/ravahn/FFXIV_ACT_Plugin/releases/latest` 回應。
    const LATEST_FIXTURE: &str = include_str!("testdata/ffxiv_act_plugin_latest_2026-10-14.json");

    fn v(text: &str) -> Version {
        Version::parse(text).expect("version")
    }

    #[test]
    fn version_accepts_only_four_numeric_fields() {
        assert_eq!(v("3.0.3.3"), Version([3, 0, 3, 3]));
        assert_eq!(v("3.0.10.0042"), Version([3, 0, 10, 42]));
        assert_eq!(v("3.0.3.3").to_string(), "3.0.3.3");
        for bad in [
            "", "3.0.3", "3.0.3.3.1", "v3.0.3.3", "3.0.3.3-beta", " 3.0.3.3", "3.0.3.3 ", "3..3.3", "3.0.3.", "+3.0.3.3",
            "3.0.3.x", "latest", "99999999999.0.0.0",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn version_compares_each_field_numerically() {
        assert!(v("3.0.10.0") > v("3.0.9.9"));
        assert!(v("3.0.3.3") > v("3.0.3.1"));
        assert!(v("4.0.0.0") > v("3.99.99.99"));
        assert!(is_outdated(v("3.0.3.1"), Some(v("3.0.3.3"))));
        assert!(!is_outdated(v("3.0.3.3"), Some(v("3.0.3.3"))), "相同就是最新版");
        assert!(!is_outdated(v("3.0.4.0"), Some(v("3.0.3.3"))), "比 GitHub 新（例如測試版）也不提醒");
        assert!(!is_outdated(v("3.0.3.1"), None), "查不到最新版時不提醒");
    }

    #[test]
    fn latest_release_fixture_yields_tag_version() {
        assert_eq!(parse_latest(LATEST_FIXTURE.as_bytes()).expect("parse"), v("3.0.3.3"));
    }

    #[test]
    fn latest_release_with_other_tag_formats_is_a_failure() {
        let tag = r#""tag_name":"3.0.3.3""#;
        assert!(LATEST_FIXTURE.contains(tag));
        for other in ["v3.0.3.3", "3.0.3", "3.0.3.3.1", "Version 3.0.3.3", ""] {
            let body = LATEST_FIXTURE.replace(tag, &format!(r#""tag_name":"{other}""#));
            assert!(matches!(parse_latest(body.as_bytes()), Err(AppError::Protocol { .. })), "{other:?}");
        }
        let no_tag = LATEST_FIXTURE.replace(tag, r#""tag":"3.0.3.3""#);
        assert!(parse_latest(no_tag.as_bytes()).is_err());
        assert!(parse_latest(br#"{"message":"API rate limit exceeded"}"#).is_err());
        assert!(parse_latest(b"<html>").is_err());
    }

    /// 回傳 `result` 並計數的假查詢。
    fn counting(calls: &AtomicUsize, result: Option<Version>) -> impl Future<Output = AppResult<Version>> + '_ {
        calls.fetch_add(1, Ordering::SeqCst);
        async move { result.ok_or_else(|| AppError::protocol(LATEST_CONTEXT)) }
    }

    #[tokio::test]
    async fn latest_is_cached_for_an_hour_unless_forced() {
        let cache = LatestCache::default();
        let calls = AtomicUsize::new(0);
        let t0 = Instant::now();
        let minutes = |m: u64| t0 + Duration::from_secs(m * 60);
        assert_eq!(cache.get_at(t0, false, || counting(&calls, Some(v("3.0.3.1")))).await, Some(v("3.0.3.1")));
        assert_eq!(cache.get_at(minutes(59), false, || counting(&calls, Some(v("3.0.3.3")))).await, Some(v("3.0.3.1")));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "一小時內沿用");
        assert_eq!(cache.get_at(minutes(59), true, || counting(&calls, Some(v("3.0.3.3")))).await, Some(v("3.0.3.3")));
        assert_eq!(calls.load(Ordering::SeqCst), 2, "重新檢查一定重查");
        // 強制重查也重新起算一小時。
        assert_eq!(cache.get_at(minutes(118), false, || counting(&calls, None)).await, Some(v("3.0.3.3")));
        assert_eq!(cache.get_at(minutes(120), false, || counting(&calls, None)).await, None);
        assert_eq!(calls.load(Ordering::SeqCst), 3, "滿一小時才重查");
        // 失敗也記一小時，不連續重試。
        assert_eq!(cache.get_at(minutes(150), false, || counting(&calls, Some(v("3.0.3.3")))).await, None);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    /// 可攜式 ACT（設定在 exe 資料夾的 `Config\`），插件以相對路徑登記，插件檔是帶版本資源的系統 DLL。
    fn portable_act(root: &Path, plugin: Option<&Path>) -> PathBuf {
        let exe = root.join("Advanced Combat Tracker.exe");
        fs::create_dir_all(root.join("Config")).expect("mkdir config");
        fs::write(&exe, b"MZ act").expect("write exe");
        let entry = plugin.map_or(String::new(), |rel| {
            let dll = root.join(rel);
            fs::create_dir_all(dll.parent().expect("plugin dir")).expect("mkdir plugin");
            let system = Path::new(&std::env::var_os("SystemRoot").expect("SystemRoot")).join(r"System32\kernel32.dll");
            fs::copy(system, dll).expect("copy dll");
            format!(r#"<Plugin Enabled="True" Path="{}" />"#, rel.display())
        });
        fs::write(
            root.join(r"Config\Advanced Combat Tracker.config.xml"),
            format!("<Config><ActPlugins>{entry}</ActPlugins></Config>"),
        )
        .expect("write config");
        exe
    }

    #[test]
    fn versions_read_file_version_once_per_act_and_compare_with_latest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = Env {
            appdata: dir.path().join("Roaming"),
            home: dir.path().join("home"),
            act_images: HashSet::new(),
        };
        let settings = |exe: &Path| ActSettings {
            path: Some(exe.to_path_buf()),
            launch_with_game: false,
        };
        let exe = portable_act(&dir.path().join("ACT"), Some(Path::new(r"Plugins\FFXIV_ACT_Plugin\FFXIV_ACT_Plugin.dll")));
        let dll = dir.path().join(r"ACT\Plugins\FFXIV_ACT_Plugin\FFXIV_ACT_Plugin.dll");
        let local = installed_version(&dll).expect("系統 DLL 的 FileVersion");
        let [a, b, c, d] = local.0;
        let newer = Version([a, b, c, d + 1]);

        // 兩服同一個 ACT（大小寫不同也算同一個）：狀態相同。
        let upper = PathBuf::from(exe.to_string_lossy().to_uppercase());
        let shared = ActConfig {
            global: settings(&exe),
            tw: settings(&upper),
        };
        let out = versions(&shared, &env, Some(newer));
        let expected = PluginStatus::Installed {
            version: local.to_string(),
            outdated: true,
        };
        assert_eq!((&out.global, &out.tw), (&expected, &expected));
        assert_eq!(out.latest, Some(newer.to_string()));
        let same = versions(&shared, &env, Some(local));
        assert!(matches!(same.global, PluginStatus::Installed { outdated: false, .. }));
        let offline = versions(&shared, &env, None);
        assert!(matches!(offline.tw, PluginStatus::Installed { outdated: false, .. }));
        assert_eq!(offline.latest, None);

        // 另一服：沒登記插件；之後改成登記一個沒有版本資源的檔案；沒設定。
        let other = portable_act(&dir.path().join("ACT2"), None);
        let config = ActConfig {
            global: settings(&exe),
            tw: settings(&other),
        };
        assert_eq!(versions(&config, &env, Some(newer)).tw, PluginStatus::NotInstalled);
        let text_dll = dir.path().join(r"ACT2\FFXIV_ACT_Plugin.dll");
        fs::write(&text_dll, b"not a PE file").expect("write dll");
        fs::write(
            dir.path().join(r"ACT2\Config\Advanced Combat Tracker.config.xml"),
            r#"<Config><ActPlugins><Plugin Enabled="False" Path="FFXIV_ACT_Plugin.dll" /></ActPlugins></Config>"#,
        )
        .expect("write config");
        assert_eq!(versions(&config, &env, Some(newer)).tw, PluginStatus::VersionUnknown);
        let only_global = ActConfig {
            global: settings(&exe),
            tw: ActSettings::default(),
        };
        assert_eq!(versions(&only_global, &env, None).tw, PluginStatus::NotConfigured);
    }
}
