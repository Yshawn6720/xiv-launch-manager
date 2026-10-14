//! 國際服登入閘道狀態（官方 frontier `gate_status.json`，XL `Game/GateStatus.cs`）。
//!
//! 2026-10-12 實測：不需特殊 header；維修中回 `{"status":0,"message":[…],"news":[…]}`，開放時
//! `status` 為 1。XL 把 `status` 當 bool，所以 0/1 與 true/false 都接受。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::USER_AGENT;
use serde::Serialize;

const GATE_STATUS_URL: &str = "https://frontier.ffxiv.com/worldStatus/gate_status.json?lang=en-gb";
/// 狀態查詢只是啟動前的提示；查不到就照常啟動，不值得久等。
const GATE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ServerStatus {
    Open,
    Maintenance,
    /// 取得失敗或格式不符；呼叫端照常繼續。
    Unknown,
}

/// 查詢國際服閘道狀態。任何網路、HTTP 或格式錯誤都回 `Unknown`。
pub async fn fetch_gate_status() -> ServerStatus {
    let Ok(client) = reqwest::Client::builder().timeout(GATE_TIMEOUT).build() else {
        return ServerStatus::Unknown;
    };
    // 與官方啟動器相同帶時間戳避開快取；時鐘早於 epoch 時用 0，仍是合法 query。
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let resp = client
        .get(format!("{GATE_STATUS_URL}&_={millis}"))
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
    match resp.bytes().await {
        Ok(body) => parse_gate_status(&body),
        Err(_) => ServerStatus::Unknown,
    }
}

fn parse_gate_status(body: &[u8]) -> ServerStatus {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return ServerStatus::Unknown;
    };
    match value.get("status") {
        Some(serde_json::Value::Bool(true)) => ServerStatus::Open,
        Some(serde_json::Value::Bool(false)) => ServerStatus::Maintenance,
        Some(serde_json::Value::Number(n)) => match n.as_u64() {
            Some(1) => ServerStatus::Open,
            Some(0) => ServerStatus::Maintenance,
            _ => ServerStatus::Unknown,
        },
        _ => ServerStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_accepts_number_or_bool_and_falls_back_to_unknown() {
        let cases: [(&[u8], ServerStatus); 9] = [
            (
                br#"{"status":0,"message":["The game is unavailable for play during maintenance."],"news":[]}"#,
                ServerStatus::Maintenance,
            ),
            (br#"{"status":1}"#, ServerStatus::Open),
            (br#"{"status":true}"#, ServerStatus::Open),
            (br#"{"status":false}"#, ServerStatus::Maintenance),
            (br#"{"status":2}"#, ServerStatus::Unknown),
            (br#"{"status":"0"}"#, ServerStatus::Unknown),
            (br#"{"message":[]}"#, ServerStatus::Unknown),
            (b"<html>", ServerStatus::Unknown),
            (b"", ServerStatus::Unknown),
        ];
        for (body, expected) in cases {
            assert_eq!(
                parse_gate_status(body),
                expected,
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }
}
