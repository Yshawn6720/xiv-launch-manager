//! 偵測舊啟動器留下的衛月資料（設定、插件、插件設定），供 profile 直接沿用原資料夾。
//!
//! 衛月的 `pluginConfigs\` 跟著 `--dalamud-configuration-path` 所在目錄，所以只要把 config 與
//! plugin 目錄指到舊資料夾，插件設定也一併沿用。偵測只讀不寫；任何讀取錯誤都只跳過該候選。

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::region::Region;

/// 一個可沿用的衛月資料夾。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectedDalamudData {
    pub label: String,
    pub region: Region,
    pub path: PathBuf,
    pub plugin_count: usize,
}

/// 讀 `APPDATA` 偵測；取不到環境變數就當作沒有舊資料。
pub fn detect_from_env() -> Vec<DetectedDalamudData> {
    std::env::var_os("APPDATA")
        .map(|appdata| detect(Path::new(&appdata)))
        .unwrap_or_default()
}

/// 依序：國際服 XIVLauncher、TC 啟動器共用、TC 啟動器各 profile。
pub fn detect(appdata: &Path) -> Vec<DetectedDalamudData> {
    let mut found = Vec::new();
    found.extend(candidate(
        "XIVLauncher".into(),
        Region::Global,
        appdata.join("XIVLauncher"),
    ));

    let tc = appdata.join("FFXIVSimpleLauncher");
    found.extend(candidate(
        "TC 啟動器（共用）".into(),
        Region::Tw,
        tc.join("Dalamud").join("Config"),
    ));

    let names = tc_profile_names(&tc.join("settings.json"));
    let mut profiles: Vec<DetectedDalamudData> = fs::read_dir(tc.join("Profiles"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let id = e.file_name().into_string().ok()?;
            let name = names
                .iter()
                .find(|(pid, _)| *pid == id)
                .map_or(id.as_str(), |(_, name)| name.as_str());
            candidate(
                format!("TC 啟動器 profile「{name}」"),
                Region::Tw,
                e.path().join("Dalamud"),
            )
        })
        .collect();
    profiles.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.path.cmp(&b.path)));
    found.extend(profiles);
    found
}

/// 目錄內有 `dalamudConfig.json` 檔案才算；路徑須能以 UTF-8 回傳前端。
fn candidate(label: String, region: Region, path: PathBuf) -> Option<DetectedDalamudData> {
    path.to_str()?;
    if !path.join("dalamudConfig.json").is_file() {
        return None;
    }
    let plugin_count = count_subdirs(&path.join("installedPlugins"))?;
    Some(DetectedDalamudData {
        label,
        region,
        path,
        plugin_count,
    })
}

/// 子目錄數；目錄不存在為 0，其他讀取錯誤回 `None`（跳過候選）。
pub(super) fn count_subdirs(dir: &Path) -> Option<usize> {
    match fs::read_dir(dir) {
        Ok(entries) => Some(
            entries
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .count(),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(0),
        Err(_) => None,
    }
}

/// TC 啟動器 `settings.json` 的 `DalamudProfiles: [{Id, Name}]`；檔案缺少或格式壞掉回空。
fn tc_profile_names(file: &Path) -> Vec<(String, String)> {
    let Ok(bytes) = fs::read(file) else {
        return Vec::new();
    };
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    let Ok(settings) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Vec::new();
    };
    settings
        .get("DalamudProfiles")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let id = p.get("Id")?.as_str()?;
            let name = p.get("Name")?.as_str()?.trim();
            (!name.is_empty()).then(|| (id.to_string(), name.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_data_dir(dir: &Path, plugins: &[&str]) {
        fs::create_dir_all(dir.join("pluginConfigs")).expect("mkdir");
        fs::write(dir.join("dalamudConfig.json"), b"{}").expect("write config");
        for p in plugins {
            fs::create_dir_all(dir.join("installedPlugins").join(p)).expect("mkdir plugin");
        }
    }

    #[test]
    fn detects_all_three_kinds_in_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let appdata = tmp.path();
        let tc = appdata.join("FFXIVSimpleLauncher");
        make_data_dir(&appdata.join("XIVLauncher"), &["A", "B"]);
        make_data_dir(&tc.join("Dalamud").join("Config"), &["C"]);
        make_data_dir(&tc.join("Profiles").join("abc").join("Dalamud"), &[]);
        make_data_dir(&tc.join("Profiles").join("def").join("Dalamud"), &["D"]);
        // installedPlugins 底下的檔案不算插件。
        fs::write(tc.join("Dalamud/Config/installedPlugins/x.json"), b"").expect("write");
        // 缺 dalamudConfig.json 的不列入。
        fs::create_dir_all(tc.join("Profiles").join("empty").join("Dalamud")).expect("mkdir");
        let mut settings = b"\xEF\xBB\xBF".to_vec();
        settings.extend_from_slice(
            r#"{"DalamudProfiles":[{"Id":"abc","Name":"1","Description":""},{"Id":"def","Name":"","Description":""}]}"#
                .as_bytes(),
        );
        fs::write(tc.join("settings.json"), settings).expect("write settings");

        let found = detect(appdata);
        let summary: Vec<_> = found
            .iter()
            .map(|d| (d.label.as_str(), d.region, d.plugin_count))
            .collect();
        assert_eq!(
            summary,
            [
                ("XIVLauncher", Region::Global, 2),
                ("TC 啟動器（共用）", Region::Tw, 1),
                ("TC 啟動器 profile「1」", Region::Tw, 0),
                ("TC 啟動器 profile「def」", Region::Tw, 1),
            ]
        );
        assert_eq!(found[0].path, appdata.join("XIVLauncher"));
        assert_eq!(found[2].path, tc.join("Profiles").join("abc").join("Dalamud"));
        let json = serde_json::to_value(&found[1]).expect("json");
        assert_eq!(json["region"], "tw");
        assert_eq!(json["pluginCount"], 1);
        assert!(json["path"].is_string());
    }

    #[test]
    fn missing_config_file_is_not_a_candidate() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("XIVLauncher");
        fs::create_dir_all(dir.join("installedPlugins").join("A")).expect("mkdir");
        // 同名目錄不是檔案。
        fs::create_dir_all(dir.join("dalamudConfig.json")).expect("mkdir");
        assert!(detect(tmp.path()).is_empty());
    }

    #[test]
    fn corrupt_settings_falls_back_to_id() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let tc = tmp.path().join("FFXIVSimpleLauncher");
        make_data_dir(&tc.join("Profiles").join("abc").join("Dalamud"), &[]);
        fs::write(tc.join("settings.json"), b"{not json").expect("write");
        let found = detect(tmp.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].label, "TC 啟動器 profile「abc」");
    }
}
