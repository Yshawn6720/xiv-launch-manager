//! 兩服官方消息的共用部分（protocols §1.4.2、§2.7）：消息項目、連結白名單、以系統瀏覽器開啟。
//! 抓取與解析各在 `global::news`（frontier headline JSON）與 `tw::news`（官網 HTML）。
//! 也負責以系統瀏覽器開啟固定網址：各服官方商城、本程式 README 的外部包示範影片。

use std::time::Duration;

use reqwest::Url;
use serde::Serialize;
use windows::Win32::UI::Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};

use crate::error::{AppError, AppResult};
use crate::region::Region;

/// 側欄顯示的則數。
pub const NEWS_LIMIT: usize = 5;
/// 只是側欄顯示用，不值得久等（與伺服器狀態相同）；每次請求各自計時。
pub const NEWS_TIMEOUT: Duration = Duration::from_secs(10);
pub const NEWS_USER_AGENT: &str = concat!("xiv-launch-manager/", env!("CARGO_PKG_VERSION"));

/// 一則消息；`url` 已通過該服的白名單。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NewsItem {
    pub title: String,
    /// `YYYY/MM/DD`（兩服顯示格式相同）。
    pub date: String,
    pub url: String,
}

/// 消息連結白名單（fail-closed）：scheme 是 `https`、host 完全等於 `hosts` 其中之一（不做子字串或後綴比對），
/// 沒有帳號密碼，也沒有非預設 port。
pub fn is_allowed(url: &Url, hosts: &[&str]) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some_and(|h| hosts.contains(&h))
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
}

/// 各服允許開啟的主機。
fn hosts(region: Region) -> &'static [&'static str] {
    match region {
        Region::Global => crate::global::NEWS_HOSTS,
        Region::Tw => crate::tw::NEWS_HOSTS,
    }
}

/// 開啟用：值必須本身就是 `region` 白名單內的絕對 `https` URL（不解析相對路徑、不升級 `http`）。
fn allowed_url(region: Region, raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    is_allowed(&url, hosts(region)).then_some(url)
}

/// 以系統預設瀏覽器開啟 `region` 的消息（`open_news` command）。不信任前端傳來的值：先以 [`allowed_url`] 再檢查
/// 一次該服白名單，不符回 [`AppError::NewsLinkRejected`]、不開啟。
pub fn open_news(region: Region, raw: &str) -> AppResult<()> {
    let url = allowed_url(region, raw).ok_or(AppError::NewsLinkRejected { region })?;
    open_in_browser(url.as_str())
}

/// 各服官方商城（protocols §1.4.3、§2.8）：國際服 Mog Station、繁中服水晶商城。
fn shop_url(region: Region) -> &'static str {
    match region {
        Region::Global => "https://secure.square-enix.com/account/app/svc/mogstation/",
        Region::Tw => "https://www.ffxiv.com.tw/web/store/index.aspx",
    }
}

/// 以系統預設瀏覽器開啟 `region` 的官方商城（`open_shop` command）；網址是 [`shop_url`] 的常數，不收前端傳來的網址。
pub fn open_shop(region: Region) -> AppResult<()> {
    open_in_browser(shop_url(region))
}

/// 公開 repo README「國際服字體與漢化包」段（示範影片放在這段）。錨點是 GitHub 依標題產生的 id，中文以 UTF-8
/// percent-encoding 寫出；README 改標題時要一起改（測試核對）。
const PACK_DEMO_URL: &str = "https://github.com/Yshawn6720/xiv-launch-manager#%E5%9C%8B%E9%9A%9B%E6%9C%8D%E5%AD%97%E9%AB%94%E8%88%87%E6%BC%A2%E5%8C%96%E5%8C%85";

/// 以系統預設瀏覽器開啟外部包示範影片（`open_pack_demo` command）；網址是常數，不收前端傳來的網址。
pub fn open_pack_demo() -> AppResult<()> {
    open_in_browser(PACK_DEMO_URL)
}

/// 以 `ShellExecuteExW`（verb `open`，與 ACT 相同）把 `url` 交給 https 的預設處理程式，不經 shell。不用開資料夾的
/// `explorer.exe <URL>`：2026-10-14 本機（Windows 11 build 26300）實測它會開出「文件」資料夾，不會開瀏覽器。
fn open_in_browser(url: &str) -> AppResult<()> {
    let file = HSTRING::from(url);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        // NOASYNC：要求呼叫返回前完成啟動（呼叫端不保證有訊息迴圈）。
        fMask: SEE_MASK_NOASYNC,
        lpVerb: w!("open"),
        lpFile: PCWSTR(file.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: info 的字串指標指向本函式持有、以 0 結尾（HSTRING 保證）且呼叫期間存活的緩衝區；cbSize 已設定；
    // 沒有 SEE_MASK_NOCLOSEPROCESS，不會回傳需要關閉的 handle。quality-allow: Win32 FFI，ShellExecuteExW 沒有安全包裝
    unsafe { ShellExecuteExW(&mut info) }.map_err(|e| AppError::Io {
        context: "開啟瀏覽器".into(),
        message: e.message(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_accepts_only_absolute_https_on_that_regions_hosts() {
        let tw = "https://www.ffxiv.com.tw/web/news/news_content.aspx?id=mP1BDEYZla";
        let global = "https://eu.finalfantasyxiv.com/lodestone/news/detail/d136a3092faff585c1caf018cca5df1bce1792f4";
        let cases: [(Region, &str, Option<&str>); 27] = [
            (Region::Tw, tw, Some(tw)),
            (Region::Tw, "HTTPS://WWW.FFXIV.COM.TW/web/news/", Some("https://www.ffxiv.com.tw/web/news/")),
            (Region::Global, global, Some(global)),
            (Region::Global, "https://jp.finalfantasyxiv.com/lodestone/", Some("https://jp.finalfantasyxiv.com/lodestone/")),
            (Region::Global, "https://na.finalfantasyxiv.com/lodestone/", Some("https://na.finalfantasyxiv.com/lodestone/")),
            (Region::Global, "https://de.finalfantasyxiv.com/lodestone/", Some("https://de.finalfantasyxiv.com/lodestone/")),
            (Region::Global, "https://fr.finalfantasyxiv.com/lodestone/", Some("https://fr.finalfantasyxiv.com/lodestone/")),
            // 另一服的主機不行。
            (Region::Tw, global, None),
            (Region::Global, tw, None),
            (Region::Tw, "http://www.ffxiv.com.tw/web/news/news_content.aspx?id=x", None),
            (Region::Tw, "/web/news/news_content.aspx?id=x", None),
            (Region::Tw, "news_content.aspx?id=x", None),
            (Region::Tw, "//www.ffxiv.com.tw/web/news/", None),
            (Region::Tw, "javascript:alert(1)", None),
            (Region::Tw, "https://evil.example/", None),
            (Region::Tw, "https://www.ffxiv.com.tw.evil.com/", None),
            (Region::Tw, "https://www.ffxiv.com.tw@evil.example/", None),
            (Region::Tw, "https://user:pw@www.ffxiv.com.tw/", None),
            (Region::Tw, "https://www.ffxiv.com.tw:8443/", None),
            (Region::Tw, "https://ffxiv.com.tw/", None),
            (Region::Tw, "file:///C:/Windows/System32/calc.exe", None),
            (Region::Tw, "", None),
            (Region::Global, "http://eu.finalfantasyxiv.com/lodestone/", None),
            (Region::Global, "https://eu.finalfantasyxiv.com.evil.com/lodestone/", None),
            (Region::Global, "https://finalfantasyxiv.com/lodestone/", None),
            (Region::Global, "https://www.finalfantasyxiv.com/", None),
            (Region::Global, "https://frontier.ffxiv.com/news/headline.json", None),
        ];
        for (region, raw, expected) in cases {
            assert_eq!(allowed_url(region, raw).map(String::from).as_deref(), expected, "{region} {raw}");
        }
    }

    #[test]
    fn shop_opens_each_regions_official_store_over_https() {
        let cases = [
            (Region::Global, "https://secure.square-enix.com/account/app/svc/mogstation/"),
            (Region::Tw, "https://www.ffxiv.com.tw/web/store/index.aspx"),
        ];
        for (region, expected) in cases {
            assert_eq!(shop_url(region), expected, "{region}");
        }
    }

    /// 錨點必須對到 README 裡唯一一個同名的二級標題：GitHub 遇到重名標題會加 `-1` 後綴，改標題則錨點失效，
    /// 兩種情況瀏覽器都只會停在頁首。
    #[test]
    fn pack_demo_opens_the_readme_section_that_exists() {
        let url = Url::parse(PACK_DEMO_URL).expect("parse");
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("github.com"));
        assert_eq!(url.path(), "/Yshawn6720/xiv-launch-manager");
        let encoded = url.fragment().expect("fragment").as_bytes();
        let mut bytes = Vec::new();
        let mut i = 0;
        while i < encoded.len() {
            if encoded[i] == b'%' {
                let hex = std::str::from_utf8(&encoded[i + 1..i + 3]).expect("hex");
                bytes.push(u8::from_str_radix(hex, 16).expect("hex"));
                i += 3;
            } else {
                bytes.push(encoded[i]);
                i += 1;
            }
        }
        let heading = format!("## {}", String::from_utf8(bytes).expect("utf8"));
        let readme = include_str!("../../README.md");
        assert_eq!(readme.lines().filter(|l| l.trim_end() == heading).count(), 1, "{heading}");
    }
}
