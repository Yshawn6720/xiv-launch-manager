//! 國際服官方公告（frontier `news/headline.json`，protocols §1.4.2）。只用於登入頁側欄顯示。
//!
//! 2026-10-14 實測：不需特殊 header（帶不帶官方啟動器的 Origin／Referer 回應相同）。回應有 `news`、`pinned`、
//! `topics`、`banner`；`news`、`pinned` 各 5 則公告，`url` 是空字串，網址依 XL `Game/Headlines.cs` 以 `id` 組成
//! 該語言 Lodestone 的公告頁。本專案只用 `news`＋`pinned`（Lodestone 的「公告」；`topics` 是特集與活動，
//! 官方啟動器另列一區），合起來依時間取最新 5 則。

use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use reqwest::Url;
use reqwest::header::USER_AGENT;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::news::{self, NEWS_LIMIT, NEWS_TIMEOUT, NEWS_USER_AGENT, NewsItem};
use crate::region::GlobalLanguage;

const HEADLINE_URL: &str = "https://frontier.ffxiv.com/news/headline.json";
/// 國際服消息連結允許的主機（2026-10-14 實測各語言回應的 `topics` 網址，以及 XL 依語言組的 Lodestone 主機）；
/// 與 scheme `https` 一起構成白名單，只接受完全相同的 host。
pub const NEWS_HOSTS: &[&str] = &[
    "jp.finalfantasyxiv.com",
    "na.finalfantasyxiv.com",
    "eu.finalfantasyxiv.com",
    "de.finalfantasyxiv.com",
    "fr.finalfantasyxiv.com",
];
const CONTEXT: &str = "國際服公告";

/// headline 的 `lang` 參數（XL `ClientLanguage.GetLangCode`，英文固定 `en-gb`，同 gate status）。
fn lang_code(language: GlobalLanguage) -> &'static str {
    match language {
        GlobalLanguage::Japanese => "ja",
        GlobalLanguage::English => "en-gb",
        GlobalLanguage::German => "de",
        GlobalLanguage::French => "fr",
    }
}

/// 該語言的 Lodestone 主機（XL `ClientLanguage.GetLangCodeLodestone`）；都在 [`NEWS_HOSTS`] 之內。
fn lodestone_host(language: GlobalLanguage) -> &'static str {
    match language {
        GlobalLanguage::Japanese => "jp.finalfantasyxiv.com",
        GlobalLanguage::English => "eu.finalfantasyxiv.com",
        GlobalLanguage::German => "de.finalfantasyxiv.com",
        GlobalLanguage::French => "fr.finalfantasyxiv.com",
    }
}

/// 讀國際服公告（`language` 是帳號的客戶端語言），回傳依時間由新到舊的最新 [`NEWS_LIMIT`] 則。網路或 HTTP 錯誤、
/// 逾時，或回應不是含 `news`、`pinned` 陣列的 JSON 時回錯誤；陣列裡沒有有效項目時回空陣列。
pub async fn fetch_news(language: GlobalLanguage) -> AppResult<Vec<NewsItem>> {
    let client = reqwest::Client::builder()
        .timeout(NEWS_TIMEOUT)
        .build()
        .map_err(|e| AppError::network(CONTEXT, e))?;
    // 與官方啟動器相同帶毫秒時間戳避開快取；時鐘早於 epoch 時用 0，仍是合法 query。
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let body = client
        .get(format!(
            "{HEADLINE_URL}?lang={}&media=pcapp&_={millis}",
            lang_code(language)
        ))
        .header(USER_AGENT, NEWS_USER_AGENT)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| AppError::network(CONTEXT, e))?
        .bytes()
        .await
        .map_err(|e| AppError::network(CONTEXT, e))?;
    parse(&body, language).ok_or_else(|| AppError::protocol(CONTEXT))
}

/// 只取用得到的兩個陣列；每一則先留成 JSON 值，個別轉換失敗時只丟那一則。
#[derive(Deserialize)]
struct Headlines {
    news: Vec<serde_json::Value>,
    pinned: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct Headline {
    date: String,
    title: String,
    url: Option<String>,
    id: Option<String>,
}

/// `pinned` 在前、`news` 在後，依時間由新到舊（同一時間保持這個順序）取前 [`NEWS_LIMIT`] 則；同一個 URL 只留
/// 第一次出現的。`None`＝不是 JSON，或缺 `news`／`pinned` 陣列（格式改版）。
fn parse(body: &[u8], language: GlobalLanguage) -> Option<Vec<NewsItem>> {
    let Headlines { news, pinned } = serde_json::from_slice(body).ok()?;
    let mut entries: Vec<(DateTime<Utc>, NewsItem)> = Vec::with_capacity(news.len() + pinned.len());
    for value in pinned.into_iter().chain(news) {
        if let Some((at, item)) = parse_item(value, language)
            && !entries.iter().any(|(_, x)| x.url == item.url)
        {
            entries.push((at, item));
        }
    }
    // 穩定排序：同一時間保持 pinned 在前的順序。
    entries.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    entries.truncate(NEWS_LIMIT);
    Some(entries.into_iter().map(|(_, item)| item).collect())
}

/// 一則 → （時間, 消息）。`date` 須是 RFC 3339（實測 `2026-10-14T08:00:00Z`），顯示為 UTC 的 `YYYY/MM/DD`；
/// `url` 非空時用它（`http` 升級成 `https`），空的時候以 `id` 組 Lodestone 公告頁；標題是空的、缺欄位、
/// 型別不符，或網址沒通過白名單時為 `None`。
fn parse_item(value: serde_json::Value, language: GlobalLanguage) -> Option<(DateTime<Utc>, NewsItem)> {
    let h: Headline = serde_json::from_value(value).ok()?;
    let at = DateTime::parse_from_rfc3339(&h.date).ok()?.with_timezone(&Utc);
    let title = h.title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        return None;
    }
    let url = match h.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        Some(raw) => absolute_url(raw)?,
        None => detail_url(h.id.as_deref()?, language)?,
    };
    Some((
        at,
        NewsItem {
            title,
            date: at.format("%Y/%m/%d").to_string(),
            url: url.into(),
        },
    ))
}

/// 回應裡的網址：只接受絕對網址（實測都是），`http` 升級成 `https`，再過白名單。
fn absolute_url(raw: &str) -> Option<Url> {
    let mut url = Url::parse(raw).ok()?;
    if url.scheme() == "http" {
        url.set_scheme("https").ok()?;
    }
    news::is_allowed(&url, NEWS_HOSTS).then_some(url)
}

/// 以 `id` 組該語言 Lodestone 的公告頁（XL `Game/Headlines.cs`）。`id` 實測是 40 位小寫 hex；只接受 hex，
/// 免得組出其他路徑。
fn detail_url(id: &str, language: GlobalLanguage) -> Option<Url> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let url = Url::parse(&format!(
        "https://{}/lodestone/news/detail/{id}",
        lodestone_host(language)
    ))
    .ok()?;
    news::is_allowed(&url, NEWS_HOSTS).then_some(url)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    const ID_A: &str = "d136a3092faff585c1caf018cca5df1bce1792f4";

    /// 實際回應 `news`／`pinned` 的一則（2026-10-14 `lang=en-gb` 原樣的欄位）。
    fn headline(date: &str, title: &str, id: &str) -> Value {
        json!({ "date": date, "title": title, "url": "", "id": id, "tag": "" })
    }

    /// 實際回應的骨架：四個 key 都在，`topics` 有網址，`banner` 是空的。
    fn body(news: Vec<Value>, pinned: Vec<Value>) -> Vec<u8> {
        json!({
            "news": news,
            "topics": [{
                "date": "2026-10-14T08:00:00Z",
                "title": "Two New Albums Now Available for Streaming, Digital Purchase, and Physical Pre-order!",
                "url": "https://eu.finalfantasyxiv.com/lodestone/topics/detail/e41da6752959f17135a8ba313faafb87bf5d899a",
                "id": "e41da6752959f17135a8ba313faafb87bf5d899a"
            }],
            "pinned": pinned,
            "banner": []
        })
        .to_string()
        .into_bytes()
    }

    fn eu(id: &str) -> String {
        format!("https://eu.finalfantasyxiv.com/lodestone/news/detail/{id}")
    }

    fn hex(n: u32) -> String {
        format!("{n:040x}")
    }

    /// 解析結果的（標題, 日期, URL）；`None`＝格式不符。
    type Parsed = Option<Vec<(String, String, String)>>;

    fn rows(rows: &[(&str, &str, &str)]) -> Parsed {
        Some(
            rows.iter()
                .map(|(t, d, u)| ((*t).to_owned(), (*d).to_owned(), (*u).to_owned()))
                .collect(),
        )
    }

    fn with_url(date: &str, title: &str, url: Value) -> Value {
        json!({ "date": date, "title": title, "url": url, "id": hex(9), "tag": "" })
    }

    #[test]
    fn parse_table() {
        // 實測 en-gb：news 5 則、pinned 5 則（含 2022、2024 年的常駐公告）；合併後依時間取最新 5 則。
        let real = body(
            vec![
                headline("2026-10-14T08:00:00Z", "FINAL FANTASY XIV Materials Usage License Revised", ID_A),
                headline("2026-10-13T22:38:00Z", "Recovery from Launcher Outage Notice (Oct. 13)", "00df21400d68f9123ab80da8cf2dd2d33c6a94c6"),
                headline("2026-10-13T11:20:00Z", "Recovery from Home World Transfer / Data Centre Travel Technical Difficulties (Oct. 13)", "81e7dbe0f06279e27ce14ef7d64c6f1b35e2e2d2"),
                headline("2026-10-13T11:00:00Z", "The Lodestone Updated (Oct. 13)", "7f41bdbdad26dac52455e7b206d041bf6cb904ff"),
                headline("2026-10-13T10:38:00Z", "Known Issue Following the Release of Patch 7.57 (Oct. 13)", "b5a252471e40249c48c2fe74b18c516c3e15077a"),
            ],
            vec![
                headline("2026-10-13T12:00:00Z", "Ceremony of Eternal Bonding Reservation Restrictions", "7cd00028fa344e0921621e2f6b56f12d37c783e2"),
                headline("2026-10-13T11:00:00Z", "Online Store / Mog Station Maintenance (Oct. 15)", "bc23f5b86158b48bcbc6e9f06eaa88df4a52e0db"),
                headline("2026-09-16T03:00:00Z", "Important Notice about Software Tokens and Software Authenticators (Sep. 16)", "3da3d603969eea8ee4444777e4989829c7da2398"),
                headline("2024-06-06T07:00:00Z", "Be Wary of Phishing Attempts via In-game Messages", "8f4b7d564094ed7f780cd2e190a7ac924330d4d8"),
                headline("2022-10-06T14:15:00Z", "Precautions for Guarding Against Unauthorised Account Access", "92e4dc386fddb2110be1582992afd7028035bce5"),
            ],
        );
        let partial = body(
            vec![
                headline("2026-10-13T00:00:00Z", "完整", &hex(1)),
                json!({ "title": "少日期", "url": "", "id": hex(2) }),
                json!({ "date": "2026-10-13T00:00:00Z", "url": "", "id": hex(3) }),
                json!({ "date": "2026-10-13T00:00:00Z", "title": "少 id 也沒有網址", "url": "" }),
                headline("10/13/2026", "日期格式不符", &hex(4)),
                headline("2026-10-13T00:00:00Z", "   ", &hex(5)),
                json!({ "date": "2026-10-13T00:00:00Z", "title": 42, "url": "", "id": hex(6) }),
                json!({ "date": "2026-10-13T00:00:00Z", "title": "url 是 null", "url": null, "id": hex(7) }),
                json!("not an object"),
            ],
            vec![],
        );
        let links = body(
            vec![
                with_url("2026-10-13T09:00:00Z", "外部主機", json!("https://evil.example/lodestone/news/detail/x")),
                with_url("2026-10-13T09:00:00Z", "javascript", json!("javascript:alert(1)")),
                with_url("2026-10-13T09:00:00Z", "協定相對", json!("//evil.example/lodestone/")),
                with_url("2026-10-13T09:00:00Z", "相對路徑", json!("/lodestone/news/detail/x")),
                with_url("2026-10-13T09:00:00Z", "相似主機", json!("https://eu.finalfantasyxiv.com.evil.com/lodestone/")),
                with_url("2026-10-13T09:00:00Z", "上層網域", json!("https://finalfantasyxiv.com/lodestone/")),
                with_url("2026-10-13T09:00:00Z", "帳號", json!("https://user@eu.finalfantasyxiv.com/lodestone/")),
                with_url("2026-10-13T09:00:00Z", "port", json!("https://eu.finalfantasyxiv.com:8443/lodestone/")),
                headline("2026-10-13T09:00:00Z", "id 不是 hex", "../../../evil"),
                with_url("2026-10-13T08:00:00Z", "http 升級", json!("http://na.finalfantasyxiv.com/lodestone/topics/detail/1")),
                with_url("2026-10-13T07:00:00Z", "同站絕對", json!(" https://jp.finalfantasyxiv.com/lodestone/special/ ")),
            ],
            vec![],
        );
        let cases: [(&str, Vec<u8>, Parsed); 7] = [
            (
                "實測結構：置頂與一般合併，依時間取最新 5 則",
                real,
                rows(&[
                    ("FINAL FANTASY XIV Materials Usage License Revised", "2026/10/14", &eu(ID_A)),
                    ("Recovery from Launcher Outage Notice (Oct. 13)", "2026/10/13", &eu("00df21400d68f9123ab80da8cf2dd2d33c6a94c6")),
                    ("Ceremony of Eternal Bonding Reservation Restrictions", "2026/10/13", &eu("7cd00028fa344e0921621e2f6b56f12d37c783e2")),
                    ("Recovery from Home World Transfer / Data Centre Travel Technical Difficulties (Oct. 13)", "2026/10/13", &eu("81e7dbe0f06279e27ce14ef7d64c6f1b35e2e2d2")),
                    // 同一時間：pinned 在前。
                    ("Online Store / Mog Station Maintenance (Oct. 15)", "2026/10/13", &eu("bc23f5b86158b48bcbc6e9f06eaa88df4a52e0db")),
                ]),
            ),
            (
                "只有部分欄位或型別不符的項目丟掉，url 是 null 時以 id 組網址",
                partial,
                rows(&[
                    ("完整", "2026/10/13", &eu(&hex(1))),
                    ("url 是 null", "2026/10/13", &eu(&hex(7))),
                ]),
            ),
            (
                "白名單：外部主機、javascript:、//evil、相對路徑、相似主機、上層網域、帳號、port、非 hex id 丟掉",
                links,
                rows(&[
                    ("http 升級", "2026/10/13", "https://na.finalfantasyxiv.com/lodestone/topics/detail/1"),
                    ("同站絕對", "2026/10/13", "https://jp.finalfantasyxiv.com/lodestone/special/"),
                ]),
            ),
            ("缺 pinned", br#"{"news":[],"topics":[]}"#.to_vec(), None),
            ("news 不是陣列", br#"{"news":{},"pinned":[]}"#.to_vec(), None),
            ("不是 JSON", b"<html>maintenance</html>".to_vec(), None),
            ("兩個陣列都是空的", body(vec![], vec![]), Some(Vec::new())),
        ];
        for (name, input, expected) in cases {
            let got = parse(&input, GlobalLanguage::English)
                .map(|items| items.into_iter().map(|n| (n.title, n.date, n.url)).collect());
            assert_eq!(got, expected, "{name}");
        }
    }

    #[test]
    fn date_is_utc_and_url_follows_language() {
        // UTC 22:38 在日本已是隔天，仍顯示來源的 UTC 日期。
        let input = body(vec![headline("2026-10-13T22:38:00Z", "ランチャー 復旧作業完了のお知らせ(10/14)", ID_A)], vec![]);
        let cases = [
            (GlobalLanguage::Japanese, "jp"),
            (GlobalLanguage::English, "eu"),
            (GlobalLanguage::German, "de"),
            (GlobalLanguage::French, "fr"),
        ];
        for (language, sub) in cases {
            let items = parse(&input, language).unwrap_or_default();
            let urls: Vec<_> = items.iter().map(|n| (n.date.as_str(), n.url.as_str())).collect();
            let expected = format!("https://{sub}.finalfantasyxiv.com/lodestone/news/detail/{ID_A}");
            assert_eq!(urls, [("2026/10/13", expected.as_str())], "{language:?}");
        }
        // 帶時差的時間轉成 UTC 再取日期。
        let offset = body(vec![headline("2026-10-14T07:00:00+09:00", "offset", ID_A)], vec![]);
        let dates: Vec<_> = parse(&offset, GlobalLanguage::English).unwrap_or_default().into_iter().map(|n| n.date).collect();
        assert_eq!(dates, ["2026/10/13"]);
    }

    #[test]
    fn same_url_in_pinned_and_news_is_listed_once() {
        let input = body(
            vec![headline("2026-10-13T12:00:00Z", "news 裡的同一則", ID_A)],
            vec![headline("2026-10-13T12:00:00Z", "置頂", ID_A)],
        );
        let titles: Vec<_> = parse(&input, GlobalLanguage::English).unwrap_or_default().into_iter().map(|n| n.title).collect();
        assert_eq!(titles, ["置頂"]);
    }
}
