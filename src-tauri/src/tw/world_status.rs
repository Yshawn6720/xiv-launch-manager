//! 台服 World 狀態（官網 `worldstatus/index.aspx`，protocols §1.4.1）。只用於側欄顯示，不擋登入。
//!
//! 2026-10-14 實測：不需特殊 header；狀態直接寫在 HTML 裡（沒有 AJAX／JSON 介面），
//! `world_status_list` 區塊每個 World 一個 `<i class="active|maint">`。頁首圖例也有
//! `<i class="maint">`，所以只看該區塊內的 `<i>`。尚未在維修期間觀察過這個頁面。

use std::time::Duration;

use reqwest::header::USER_AGENT;

use crate::global::ServerStatus;

const WORLD_STATUS_URL: &str = "https://www.ffxiv.com.tw/web/worldstatus/index.aspx";
/// 只是顯示用的提示；查不到就顯示未知，不值得久等（與國際服 gate status 相同）。
const WORLD_STATUS_TIMEOUT: Duration = Duration::from_secs(10);

/// 查詢台服 World 狀態。任何網路、HTTP 或格式錯誤都回 `Unknown`。
pub async fn fetch_world_status() -> ServerStatus {
    let Ok(client) = reqwest::Client::builder()
        .timeout(WORLD_STATUS_TIMEOUT)
        .build()
    else {
        return ServerStatus::Unknown;
    };
    let resp = client
        .get(WORLD_STATUS_URL)
        .header(
            USER_AGENT,
            concat!("xiv-launch-manager/", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await;
    let Ok(resp) = resp else {
        return ServerStatus::Unknown;
    };
    if !resp.status().is_success() {
        return ServerStatus::Unknown;
    }
    match resp.text().await {
        Ok(body) => parse_world_status(&body),
        Err(_) => ServerStatus::Unknown,
    }
}

/// 任一 World 是 `maint`＝維修；至少一個且全部 `active`＝開放；找不到區塊、沒有 World、
/// 或出現其他 class＝未知（頁面改版時寧可顯示未知，也不猜）。
fn parse_world_status(html: &str) -> ServerStatus {
    let Some(start) = html.find(r#"class="world_status_list""#) else {
        return ServerStatus::Unknown;
    };
    let rest = &html[start..];
    // 區塊內只有一層 <ul>，到第一個 </ul> 為止就是 World 清單。
    let Some(end) = rest.find("</ul>") else {
        return ServerStatus::Unknown;
    };
    let mut block = &rest[..end];
    let mut worlds = 0usize;
    let mut maintenance = false;
    while let Some(pos) = block.find("<i") {
        let after = &block[pos + 2..];
        // 只認 <i> 標籤本身，排除 <img>、<input> 等同字首的標籤。
        if !after.starts_with(|c: char| c.is_ascii_whitespace() || c == '>') {
            block = after;
            continue;
        }
        let Some(close) = after.find('>') else {
            return ServerStatus::Unknown;
        };
        match class_attr(&after[..close]) {
            Some("active") => {}
            Some("maint") => maintenance = true,
            _ => return ServerStatus::Unknown,
        }
        worlds += 1;
        block = &after[close + 1..];
    }
    match (worlds, maintenance) {
        (0, _) => ServerStatus::Unknown,
        (_, true) => ServerStatus::Maintenance,
        (_, false) => ServerStatus::Open,
    }
}

/// 標籤屬性字串中 `class="…"` 的值（前後空白去掉）；`data-class` 之類不算。
fn class_attr(attrs: &str) -> Option<&str> {
    let mut rest = attrs;
    while let Some(pos) = rest.find(r#"class=""#) {
        let preceded_by_space = rest[..pos]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_whitespace());
        let value = &rest[pos + r#"class=""#.len()..];
        if preceded_by_space {
            return value.find('"').map(|end| value[..end].trim());
        }
        rest = value;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGEND: &str = r#"<div class="icon_list">
<ul>
<li><i class="active"></i>正常運作</li>
<li><i class="maint"></i>維護中</li>
</ul>
</div>"#;

    fn world(class: &str, name: &str) -> String {
        format!(
            r#"<li>
<div class="item">
<div class="left">
<div class="status_icon">
<i class="{class}" data-tooltip="正常運作"></i>
</div>
<div class="world_name">
<span>{name}</span>
</div>
</div>
<div class="world_category ">
<span>一般</span>
</div>
</div>
</li>
"#
        )
    }

    fn page(worlds: &[(&str, &str)]) -> String {
        let items: String = worlds.iter().map(|(c, n)| world(c, n)).collect();
        format!(
            r#"<main>{LEGEND}
<div class="world_status_list">
<ul>
{items}
</ul>
</div>
<div class="notice_list"><ul><li>擁擠 World 無法建立新角色。</li></ul></div>
</main>"#
        )
    }

    #[test]
    fn status_reads_only_world_list_and_falls_back_to_unknown() {
        let all_active = [
            ("active", "伊弗利特"),
            ("active", "迦樓羅"),
            ("active", "利維坦"),
            ("active", "鳳凰"),
            ("active", "奧汀"),
            ("active", "巴哈姆特"),
            ("active", "泰坦"),
        ];
        let one_maint = [("active", "伊弗利特"), ("maint", "迦樓羅"), ("active", "泰坦")];
        let unknown_class = [("active", "伊弗利特"), ("busy", "迦樓羅")];
        let cases: [(&str, String, ServerStatus); 7] = [
            // 圖例有 maint，但清單全 active：必須是開放。
            ("all active", page(&all_active), ServerStatus::Open),
            ("one maint", page(&one_maint), ServerStatus::Maintenance),
            ("all maint", page(&[("maint", "伊弗利特"), ("maint", "泰坦")]), ServerStatus::Maintenance),
            ("missing block", format!("<main>{LEGEND}</main>"), ServerStatus::Unknown),
            ("empty list", page(&[]), ServerStatus::Unknown),
            ("unknown class", page(&unknown_class), ServerStatus::Unknown),
            ("not html", String::new(), ServerStatus::Unknown),
        ];
        for (name, html, expected) in cases {
            assert_eq!(parse_world_status(&html), expected, "{name}");
        }
    }

    #[test]
    fn i_tag_without_class_is_unknown_and_lookalike_tags_are_ignored() {
        let no_class = page(&[("active", "伊弗利特")]).replace(r#"<i class="active""#, "<i");
        assert_eq!(parse_world_status(&no_class), ServerStatus::Unknown);
        let with_img = page(&[("active", "伊弗利特")]).replace(
            "<span>伊弗利特</span>",
            r#"<img class="maint" src="x.png"><span>伊弗利特</span>"#,
        );
        assert_eq!(parse_world_status(&with_img), ServerStatus::Open);
    }
}
