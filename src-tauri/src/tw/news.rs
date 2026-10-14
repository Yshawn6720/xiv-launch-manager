//! 台服官方新聞（官網 `news/news_list.aspx`，protocols §2.7）。只用於登入頁側欄顯示。
//!
//! 2026-10-14 實測：沒有 JSON／RSS 介面，清單直接寫在 HTML 裡。第 1 頁前面是置頂消息（多半是舊的常駐文），
//! 所以一般消息讀到 5 則為止（最多讀 3 頁），再和置頂消息合起來依日期取最新 5 則。
//! 連結一律經過白名單（`https`＋[`NEWS_HOSTS`]）；開啟前 `crate::news::open_news` 會再檢查一次。

use reqwest::Url;
use reqwest::header::USER_AGENT;

use crate::error::{AppError, AppResult};
use crate::news::{self, NEWS_LIMIT, NEWS_TIMEOUT, NEWS_USER_AGENT, NewsItem};

const NEWS_LIST_URL: &str = "https://www.ffxiv.com.tw/web/news/news_list.aspx";
/// 繁中服消息連結唯一允許的主機；與 scheme `https` 一起構成白名單（protocols §2.7）。
pub const NEWS_HOSTS: &[&str] = &["www.ffxiv.com.tw"];
/// 一般消息不足 [`NEWS_LIMIT`] 則時最多讀到第幾頁（第 1 頁大半是置頂消息）。
const MAX_PAGES: u32 = 3;
const CONTEXT: &str = "繁中服消息";

/// 清單上的一則；`pinned`＝置頂（`badge top`），只用來判斷要不要讀下一頁。
#[derive(Debug)]
struct Entry {
    item: NewsItem,
    pinned: bool,
}

/// 讀官網新聞清單，回傳依日期由新到舊的最新 [`NEWS_LIMIT`] 則。第 1 頁發生網路或 HTTP 錯誤、逾時，
/// 或找不到清單區塊時回錯誤；清單區塊存在但沒有有效項目時回空陣列（前端據此分辨「取不到」與「沒有消息」）。
pub async fn fetch_news() -> AppResult<Vec<NewsItem>> {
    let client = reqwest::Client::builder()
        .timeout(NEWS_TIMEOUT)
        .build()
        .map_err(|e| AppError::network(CONTEXT, e))?;
    let mut entries = parse_page(&get_page(&client, 1).await?).ok_or_else(|| AppError::protocol(CONTEXT))?;
    for page in 2..=MAX_PAGES {
        if regular_count(&entries) >= NEWS_LIMIT {
            break;
        }
        // 後面的頁只用來補足一般消息：讀不到或格式不符時，就用已經讀到的部分。
        let Ok(html) = get_page(&client, page).await else {
            break;
        };
        match parse_page(&html) {
            Some(more) if !more.is_empty() => entries.extend(more),
            _ => break,
        }
    }
    Ok(latest(entries))
}

async fn get_page(client: &reqwest::Client, page: u32) -> AppResult<String> {
    let url = if page == 1 {
        NEWS_LIST_URL.to_owned()
    } else {
        format!("{NEWS_LIST_URL}?page={page}")
    };
    client
        .get(url)
        .header(USER_AGENT, NEWS_USER_AGENT)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| AppError::network(CONTEXT, e))?
        .text()
        .await
        .map_err(|e| AppError::network(CONTEXT, e))
}

fn regular_count(entries: &[Entry]) -> usize {
    entries.iter().filter(|e| !e.pinned).count()
}

/// 依日期由新到舊（同一天保持頁面順序）取前 [`NEWS_LIMIT`] 則；同一個 URL 只留第一次出現的。
fn latest(entries: Vec<Entry>) -> Vec<NewsItem> {
    let mut items: Vec<NewsItem> = Vec::with_capacity(entries.len());
    for Entry { item, .. } in entries {
        if !items.iter().any(|x| x.url == item.url) {
            items.push(item);
        }
    }
    // `YYYY/MM/DD` 固定寬度，字串比較就是日期比較；sort_by 是穩定排序。
    items.sort_by(|a, b| b.date.cmp(&a.date));
    items.truncate(NEWS_LIMIT);
    items
}

/// 一頁清單的每一則（頁面順序）。`None`＝找不到 `news_list` 區塊或區塊沒有結尾（頁面改版或不是新聞頁）。
/// 缺欄位或連結不在白名單內的項目直接丟掉。
fn parse_page(html: &str) -> Option<Vec<Entry>> {
    let base = Url::parse(NEWS_LIST_URL).ok()?;
    let (_, after) = find_tag(html, "div", |a| has_class(a, "news_list"))?;
    let list = div_inner(after)?;
    let mut entries = Vec::new();
    let mut rest = list;
    while let Some((_, after)) = find_tag(rest, "div", |a| has_class(a, "item")) {
        let Some(item) = div_inner(after) else {
            break;
        };
        entries.extend(parse_item(item, &base));
        rest = &after[item.len()..];
    }
    Some(entries)
}

/// `<div class="item">` 的內容 → 一則。標題取 `title` 區塊裡第一個帶 `href` 的 `<a>` 的文字，日期取
/// `publish_date` 區塊（必須是 `YYYY/MM/DD`）；任一個缺少、標題是空的，或連結沒通過白名單時為 `None`。
fn parse_item(item: &str, base: &Url) -> Option<Entry> {
    let (_, after) = find_tag(item, "div", |a| has_class(a, "title"))?;
    let title_block = div_inner(after)?;
    let pinned = find_tag(title_block, "span", |a| has_class(a, "badge") && has_class(a, "top")).is_some();
    let (link_attrs, link_body) = find_tag(title_block, "a", |a| attr(a, "href").is_some())?;
    let url = resolve_href(&decode_entities(attr(link_attrs, "href")?), base)?;
    let title = clean_text(&link_body[..link_body.find("</a>")?]);
    let (_, after) = find_tag(item, "div", |a| has_class(a, "publish_date"))?;
    let date = clean_text(div_inner(after)?);
    if title.is_empty() || !is_date(&date) {
        return None;
    }
    Some(Entry {
        item: NewsItem {
            title,
            date,
            url: url.into(),
        },
        pinned,
    })
}

/// 清單上的 `href`（已解碼 entity）→ 白名單內的絕對 URL：以列表頁為 base，依 WHATWG 規則解析
/// （相對路徑、`/…`、`//host/…` 都和瀏覽器一樣），`http` 升級成 `https`；不在白名單內時為 `None`。
fn resolve_href(href: &str, base: &Url) -> Option<Url> {
    let mut url = base.join(href).ok()?;
    if url.scheme() == "http" {
        url.set_scheme("https").ok()?;
    }
    news::is_allowed(&url, NEWS_HOSTS).then_some(url)
}

/// 第一個名稱是 `name`、屬性符合 `want` 的開始標籤：回傳（屬性字串, `>` 之後的內容）。
/// 只認這個標籤本身（找 `<a` 時不會配到 `<abbr>`）；標籤沒有 `>` 時為 `None`。
fn find_tag<'a>(html: &'a str, name: &str, want: impl Fn(&str) -> bool) -> Option<(&'a str, &'a str)> {
    let mut rest = html;
    loop {
        let pos = rest.find('<')?;
        let after = &rest[pos + 1..];
        let Some(tail) = after.strip_prefix(name) else {
            rest = after;
            continue;
        };
        if !tail.starts_with(|c: char| c.is_ascii_whitespace() || c == '>') {
            rest = after;
            continue;
        }
        let close = tail.find('>')?;
        let (attrs, body) = (&tail[..close], &tail[close + 1..]);
        if want(attrs) {
            return Some((attrs, body));
        }
        rest = body;
    }
}

/// `html` 從某個 `<div …>` 的 `>` 之後開始：回傳到對應 `</div>` 之前的內容（計入巢狀的 `<div>`）；
/// 找不到對應的結尾時為 `None`。
fn div_inner(html: &str) -> Option<&str> {
    let mut depth = 0usize;
    let mut pos = 0;
    loop {
        let rest = &html[pos..];
        let close = rest.find("</div>")?;
        let before_close = &rest[..close];
        match find_tag(before_close, "div", |_| true) {
            Some((_, body)) => {
                depth += 1;
                // body 一路到 before_close 結尾，所以開始標籤之後的位置＝長度差。
                pos += before_close.len() - body.len();
            }
            None if depth == 0 => return Some(&html[..pos + close]),
            None => {
                depth -= 1;
                pos += close + "</div>".len();
            }
        }
    }
}

/// 屬性字串裡 `name="…"`（或單引號）的值；`=` 前後可以有空白，`data-name` 之類不算，沒有引號的值不接受。
fn attr<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = attrs;
    while let Some(pos) = rest.find(name) {
        let preceded_by_space = rest[..pos]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_whitespace());
        let after = &rest[pos + name.len()..];
        if preceded_by_space && let Some(value) = after.trim_start().strip_prefix('=') {
            let value = value.trim_start();
            let quote = value.chars().next().filter(|&q| q == '"' || q == '\'')?;
            let value = &value[1..];
            return value.find(quote).map(|end| &value[..end]);
        }
        rest = after;
    }
    None
}

/// 屬性字串的 class 是否含 `token`（以空白分隔的整個字，`title` 不會配到 `item_title`）。
fn has_class(attrs: &str, token: &str) -> bool {
    attr(attrs, "class").is_some_and(|c| c.split_ascii_whitespace().any(|t| t == token))
}

/// 標籤內的文字：去掉內層標籤、解碼 entity，再把連續的 ASCII 空白合成一格並去掉頭尾。
fn clean_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        text.push_str(&rest[..lt]);
        rest = rest[lt..].find('>').map_or("", |gt| &rest[lt + gt + 1..]);
    }
    text.push_str(rest);
    decode_entities(&text).split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

/// 解碼常見的 HTML entity：`&amp;` `&lt;` `&gt;` `&quot;` `&apos;` `&nbsp;`（當成一般空白）與數字形式
/// （`&#39;`、`&#x2605;`）；其他（含控制字元的數字形式）原樣保留。
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        // entity 名稱很短；分號太遠就不是 entity。
        let decoded = tail
            .find(';')
            .filter(|&end| end <= 10)
            .and_then(|end| entity(&tail[1..end]).map(|c| (c, end)));
        match decoded {
            Some((c, end)) => {
                out.push(c);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let digits = name.strip_prefix('#')?;
            let code = match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => digits.parse().ok()?,
            };
            char::from_u32(code).filter(|c| !c.is_control())
        }
    }
}

/// `YYYY/MM/DD`（只檢查格式）。
fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, &c)| match i {
            4 | 7 => c == b'/',
            _ => c.is_ascii_digit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 實際頁面的一則（2026-10-14 `news_list.aspx` 原樣的排版與 class，整則在同一行）。
    fn item(id: u32, kind: &str, pinned: bool, href: &str, title: &str, date: &str) -> String {
        let badge = if pinned { r#"<span class="badge top">置頂</span>"# } else { "" };
        format!(
            r#"<div class="item">     <div class="first_block">        <div class="news_id">{id}</div>        <div class="type {kind}">            <div class="type_icon"><svg></svg></div>        </div>    </div>    <div class="second_block">        <div class="title">{badge}<a href="{href}">{title}</a></div>        <div class="publish_date">{date}</div>        <div class="view_count">4408</div>    </div></div>  "#
        )
    }

    /// 實際頁面的骨架：頁首選單也有 `news_content.aspx` 連結與 `title` 之外的區塊，清單前有表頭列，後有分頁列。
    fn page(items: &[String]) -> String {
        let items = items.concat();
        format!(
            r#"<html><body><nav><ul class="main_nav"><li class="nav_news"><div class="sub_nav"><ul>
<li><a href="https://www.ffxiv.com.tw/web/news/news_content.aspx?id=navonly"><p>選單裡的消息</p></a></li>
</ul></div></li></ul></nav>
<div class="list news_list">

                    <div class="item_title">
                        <div class="first_block">
                            <div class="news_id">編號</div>
                            <div class="type">分類</div>
                        </div>
                        <div class="second_block">
                            <div class="title">標題</div>
                            <div class="publish_date">日期</div>
                            <div class="view_count">瀏覽數</div>
                        </div>
                    </div>

                     {items}

                </div>
                <ul class="pagination">
                    <li class="btn_page on"><a href = "javascript:void(0);" >1</a></li><li class="btn_page"><a href = "/web/news/news_list.aspx?page=2">2</a></li>
                </ul>
<div class="footer"><div class="title">頁尾標題</div></div>
</body></html>"#
        )
    }

    fn news(path: &str) -> String {
        format!("https://www.ffxiv.com.tw{path}")
    }

    fn regular(n: u32, date: &str) -> String {
        item(n, "event", false, &format!("/web/news/news_content.aspx?id=r{n}"), &format!("一般 {n}"), date)
    }

    /// 解析結果的（標題, 日期, URL）；`None`＝找不到清單區塊。
    type Parsed = Option<Vec<(String, String, String)>>;

    fn parse(html: &str) -> Parsed {
        parse_page(html).map(|entries| {
            latest(entries)
                .into_iter()
                .map(|n| (n.title, n.date, n.url))
                .collect()
        })
    }

    fn rows(rows: &[(&str, &str, &str)]) -> Parsed {
        Some(
            rows.iter()
                .map(|(t, d, u)| ((*t).to_owned(), (*d).to_owned(), (*u).to_owned()))
                .collect(),
        )
    }

    #[test]
    fn parse_table() {
        let seven = page(&[
            regular(1, "2026/10/13"),
            regular(2, "2026/10/12"),
            regular(3, "2026/10/11"),
            regular(4, "2026/10/10"),
            regular(5, "2026/10/09"),
            regular(6, "2026/10/08"),
            regular(7, "2026/10/07"),
        ]);
        let entities = page(&[item(
            9,
            "other",
            false,
            "/web/news/news_content.aspx?id=e1&amp;from=list",
            "A&amp;B &lt;C&gt; &quot;D&quot; &#39;E&#39; &#x2605; F&nbsp;&nbsp;G &copy; &#0; AT&T",
            "2026/10/01",
        )]);
        let partial = page(&[
            regular(1, "2026/10/13"),
            // 少日期
            regular(2, "2026/10/12").replace(r#"<div class="publish_date">2026/10/12</div>"#, ""),
            // 日期格式不符
            regular(3, "10/11"),
            // 少標題區塊
            regular(4, "2026/10/10").replace(r#"<div class="title">"#, r#"<div class="subject">"#),
            // 標題沒有連結
            regular(5, "2026/10/09").replace(r#"<a href="/web/news/news_content.aspx?id=r5">一般 5</a>"#, "一般 5"),
            // 標題是空的
            regular(6, "2026/10/08").replace("一般 6", "  <b> </b> "),
            regular(7, "2026/10/07"),
        ]);
        let rejected = page(&[
            item(1, "other", false, "https://evil.example/web/news/news_content.aspx?id=x", "外部主機", "2026/10/13"),
            item(2, "other", false, "javascript:alert(1)", "javascript", "2026/10/13"),
            item(3, "other", false, "  JavaScript:alert(1)", "javascript 大小寫", "2026/10/13"),
            item(4, "other", false, "//evil.example/web/news/news_content.aspx?id=x", "協定相對", "2026/10/13"),
            item(5, "other", false, "https://www.ffxiv.com.tw.evil.com/web/news/", "相似主機", "2026/10/13"),
            item(6, "other", false, "https://user@www.ffxiv.com.tw/web/news/", "帳號", "2026/10/13"),
            item(7, "other", false, "https://www.ffxiv.com.tw:8443/web/news/", "port", "2026/10/13"),
            item(8, "other", false, "ftp://www.ffxiv.com.tw/web/news/", "ftp", "2026/10/13"),
            item(9, "other", false, "http://www.ffxiv.com.tw/web/special/a.html", "http 升級", "2026/10/12"),
            item(10, "other", false, "news_content.aspx?id=rel", "相對路徑", "2026/10/11"),
            item(11, "other", false, "https://www.ffxiv.com.tw/web/special/b.html", "同站絕對", "2026/10/10"),
        ]);
        let empty_list = page(&[]);
        let cases: [(&str, String, Parsed); 7] = [
            (
                "7 則只取最新 5 則",
                seven,
                rows(&[
                    ("一般 1", "2026/10/13", &news("/web/news/news_content.aspx?id=r1")),
                    ("一般 2", "2026/10/12", &news("/web/news/news_content.aspx?id=r2")),
                    ("一般 3", "2026/10/11", &news("/web/news/news_content.aspx?id=r3")),
                    ("一般 4", "2026/10/10", &news("/web/news/news_content.aspx?id=r4")),
                    ("一般 5", "2026/10/09", &news("/web/news/news_content.aspx?id=r5")),
                ]),
            ),
            (
                "entity 解碼（標題與 href）",
                entities,
                rows(&[(
                    "A&B <C> \"D\" 'E' ★ F G &copy; &#0; AT&T",
                    "2026/10/01",
                    &news("/web/news/news_content.aspx?id=e1&from=list"),
                )]),
            ),
            ("缺少清單區塊", r#"<div class="list other_list"><div class="item"></div></div>"#.to_owned(), None),
            ("清單區塊沒有結尾", r#"<div class="list news_list"><div class="item_title">"#.to_owned(), None),
            (
                "只有部分欄位的項目丟掉",
                partial,
                rows(&[
                    ("一般 1", "2026/10/13", &news("/web/news/news_content.aspx?id=r1")),
                    ("一般 7", "2026/10/07", &news("/web/news/news_content.aspx?id=r7")),
                ]),
            ),
            (
                "白名單：外部主機、javascript:、//evil、相似主機、帳號、port、ftp 丟掉；http 升級、相對路徑以列表頁為 base",
                rejected,
                rows(&[
                    ("http 升級", "2026/10/12", &news("/web/special/a.html")),
                    ("相對路徑", "2026/10/11", &news("/web/news/news_content.aspx?id=rel")),
                    ("同站絕對", "2026/10/10", &news("/web/special/b.html")),
                ]),
            ),
            ("清單是空的", empty_list, Some(Vec::new())),
        ];
        for (name, html, expected) in cases {
            assert_eq!(parse(&html), expected, "{name}");
        }
    }

    /// 實測第 1 頁：6 則置頂（多半是舊文章）＋3 則一般；第 2 頁 9 則一般。合起來依日期取最新 5 則，
    /// 近期的置頂公告會排進來，舊的置頂文不會。
    #[test]
    fn pinned_and_regular_merge_by_date_across_pages() {
        let first = page(&[
            item(191, "other", true, "/web/news/news_content.aspx?id=mWZZW52jv4", "反詐騙宣導與公平遊戲提醒", "2026/09/08"),
            item(3, "other", true, "/web/news/news_content.aspx?id=LENEEYbBA", "一步步帶您踏入FINAL FANTASY XIV 繁體中文版的世界！", "2025/08/09"),
            item(46, "other", true, "/web/news/news_content.aspx?id=KjYX22vb6R4Y", "主程式下載與安裝、開卡教學", "2025/11/17"),
            item(65, "other", true, "/web/news/news_content.aspx?id=zLzwvvOGee2G", "跨界傳送功能教學", "2025/11/27"),
            item(244, "other", true, "/web/news/news_content.aspx?id=mWNJg02KJ4", "暗影侵襲、曦光守護產包線上開賣！詩學神典包復刻販售", "2026/09/29"),
            item(245, "other", true, "/web/news/news_content.aspx?id=mP1G3GRGZa", "自 7.25 版本發布確認到的問題", "2026/09/30"),
            item(249, "event", false, "/web/news/news_content.aspx?id=mP1BDEYZla", "光之冒險錄 FFXIV 繁中版一週年展", "2026/10/13"),
            item(250, "event", false, "/web/news/news_content.aspx?id=mWNBDDyge4", "10/20 莫古莫古★大收集～奇譚探究者～", "2026/10/13"),
            item(246, "other", false, "/web/news/news_content.aspx?id=mP1Ny21Qea", "10/06 商城新品上架", "2026/10/05"),
        ]);
        let second = page(&[
            item(248, "other", false, "/web/news/news_content.aspx?id=mP1WjVrloa", "遊戲連線異常", "2026/10/03"),
            item(243, "event", false, "/web/news/news_content.aspx?id=mP1GWXlYZa", "週年徵稿比賽", "2026/09/29"),
            item(240, "other", false, "https://www.ffxiv.com.tw/web/special/patchnote_log/patch_7.25_notes.html", "7.25更新筆記", "2026/09/23"),
        ]);
        let mut entries = parse_page(&first).unwrap_or_default();
        assert_eq!(entries.len(), 9);
        assert_eq!(regular_count(&entries), 3, "第 1 頁一般消息不足 5 則，要讀下一頁");
        entries.extend(parse_page(&second).unwrap_or_default());
        assert_eq!(regular_count(&entries), 6);
        let titles: Vec<_> = latest(entries).into_iter().map(|n| (n.title, n.date)).collect();
        assert_eq!(
            titles,
            [
                ("光之冒險錄 FFXIV 繁中版一週年展", "2026/10/13"),
                ("10/20 莫古莫古★大收集～奇譚探究者～", "2026/10/13"),
                ("10/06 商城新品上架", "2026/10/05"),
                ("遊戲連線異常", "2026/10/03"),
                ("自 7.25 版本發布確認到的問題", "2026/09/30"),
            ]
            .map(|(t, d)| (t.to_owned(), d.to_owned()))
        );
    }

    #[test]
    fn same_url_on_two_pages_is_listed_once() {
        let a = page(&[regular(1, "2026/10/13"), regular(2, "2026/10/12")]);
        let b = page(&[regular(2, "2026/10/12"), regular(3, "2026/10/11")]);
        let mut entries = parse_page(&a).unwrap_or_default();
        entries.extend(parse_page(&b).unwrap_or_default());
        let urls: Vec<_> = latest(entries).into_iter().map(|n| n.url).collect();
        assert_eq!(
            urls,
            [1, 2, 3].map(|n| news(&format!("/web/news/news_content.aspx?id=r{n}")))
        );
    }

    #[test]
    fn tag_and_attr_scanning() {
        // `<a` 不配 `<abbr>`；`href = "…"` 有空白也接；`data-href` 不算；沒有引號的值不接受。
        let (attrs, _) = find_tag(r#"<abbr href="x"><a data-href="y" href = "z">"#, "a", |_| true).unwrap_or_default();
        assert_eq!(attr(attrs, "href"), Some("z"));
        assert_eq!(attr(" href=z", "href"), None);
        assert_eq!(attr(" href='q'", "href"), Some("q"));
        assert!(has_class(r#" class="title new""#, "title"));
        assert!(!has_class(r#" class="item_title""#, "title"));
        // 巢狀 div 找到對應的結尾。
        assert_eq!(div_inner("a<div>b<div x>c</div></div>d</div>e"), Some("a<div>b<div x>c</div></div>d"));
        assert_eq!(div_inner("<div>"), None);
        assert!(is_date("2026/10/13"));
        assert!(!is_date("2026-10-13"));
        assert!(!is_date("2026/1/13"));
    }
}
