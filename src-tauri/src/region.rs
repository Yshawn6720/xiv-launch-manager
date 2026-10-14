//! 區服。新增區服＝加一個 variant，所有 exhaustive `match` 會在編譯時列出待補處。

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Region {
    Global,
    Tw,
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Region::Global => "國際服",
            Region::Tw => "繁中服",
        })
    }
}

impl Region {
    /// 新建設定檔的衛月注入延遲預設值（毫秒）；前端 `src/main.ts` 的 `DEFAULT_INJECTION_DELAY_MS` 須同步。
    /// - 國際服：launch 注入，0＝不延遲。
    /// - 繁中服：attach 注入，runner 先等 `max(延遲, 3000)`；0 會再改用
    ///   `--dalamud-delay-initialize=10000`，設 5000 插件約快 3 秒出現，也與 TC 啟動器一致。
    ///
    /// 只影響新建設定檔；已存設定檔的值與 `0` 的語意不變。
    pub const fn default_injection_delay_ms(self) -> u32 {
        match self {
            Region::Global => 0,
            Region::Tw => 5000,
        }
    }
}

/// 國際服客戶端語言（遊戲參數 `language=` 與衛月 `--dalamud-client-language=` 共用同一編號）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum GlobalLanguage {
    Japanese,
    #[default]
    English,
    German,
    French,
}

impl GlobalLanguage {
    pub fn code(self) -> u8 {
        match self {
            GlobalLanguage::Japanese => 0,
            GlobalLanguage::English => 1,
            GlobalLanguage::German => 2,
            GlobalLanguage::French => 3,
        }
    }
}

/// 台服衛月的客戶端語言編號（yanmucorp fork 的繁中，protocols §2.5）。
pub const TW_DALAMUD_LANGUAGE: u8 = 4;
