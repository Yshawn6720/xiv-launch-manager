//! 台服登入、版本檢查與啟動參數（protocols §2.1–§2.3）。版本檢查回 patch list 時把 Content-Type 與
//! body 交回呼叫端（§5.1）。
//!
//! 不變量：帳密、OTP、reCAPTCHA token、登入 token、sessionId 只在記憶體；錯誤只帶
//! 分類與 HTTP 狀態，唯一例外是伺服器 `error` 欄位（給使用者看的錯誤字串）。

use std::fmt;
use std::path::Path;
use std::time::Duration;

use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, CONTENT_TYPE, USER_AGENT};
use reqwest::{Client, ClientBuilder};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::game;
use crate::patch::update::{self, RawPatchList, VersionCheck};
use crate::region::Region;

const LOGIN_URL: &str = "https://user.ffxiv.com.tw/api/login/launcherLogin";
const SESSION_URL: &str = "https://user.ffxiv.com.tw/api/login/launcherSession";
const VERSION_CHECK_URL: &str =
    "http://patch-gamever.ffxiv.com.tw/http/win32/ffxivtc_release_tc_game/";
/// 版本檢查回應含其中之一＝有 patch 要下載（protocols §2.3）。實測清單是 http；https 也接受，兩者下載時
/// 都改走 HTTPS（`patch::list`）。
const PATCH_URL_MARKERS: [&str; 2] = ["http://patch-dl.ffxiv.com.tw", "https://patch-dl.ffxiv.com.tw"];

const TW_USER_AGENT: &str = "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko; compatible; Orbit/1.0)";
const TW_ACCEPT_LANGUAGE: &str = "en-US, en";
const TW_ACCEPT: &str = "*/*";
/// 同 TC（.NET `StringContent(json, UTF8, "application/json")`）帶 charset；reqwest `.json()` 只給 `application/json`。
const TW_JSON_CONTENT_TYPE: &str = "application/json; charset=utf-8";

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// 台服登入結果。`session_id` 會以明文進遊戲命令列（protocols §2.2），其他地方不得輸出。
pub struct TwSession {
    pub session_id: String,
}

impl fmt::Debug for TwSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TwSession")
            .field("session_id", &"<redacted>")
            .finish()
    }
}

/// 字串的 UTF-8 bytes 轉小寫 hex（launcherLogin 的 email／password 格式）。
pub fn hex_encode(s: &str) -> String {
    hex::encode(s.as_bytes())
}

/// 遊戲明文參數（protocols §2.2）。`login` 保證 session_id 不含空白，參數不會被切開。
pub fn launch_args(session: &TwSession, extra: &[(String, String)]) -> String {
    let mut args = format!(
        "DEV.LobbyHost01=neolobby01.ffxiv.com.tw DEV.LobbyPort01=54994 \
         DEV.GMServerHost=frontier.ffxiv.com.tw DEV.TestSID={} SYS.resetConfig=0 \
         DEV.SaveDataBankHost=config-dl.ffxiv.com.tw",
        session.session_id
    );
    for (key, value) in extra {
        args.push_str(&format!(" {key}={value}"));
    }
    args
}

/// launcherLogin → 版本檢查 → launcherSession（protocols §2.1）。三個請求共用 [`login_client`]（同 TC 共用一個
/// HttpClient）。版本檢查回 patch list 時不取 session，回 [`VersionCheck::PatchList`]。`ffxivgame.ver` 缺失時在
/// 送出任何請求前就回 [`AppError::VersionFileMissing`]。
pub async fn login(
    install: &Path,
    email: &str,
    password: &str,
    otp: Option<&str>,
    recaptcha_token: &str,
) -> AppResult<VersionCheck<TwSession>> {
    let (game_ver, version_body) = version_check_inputs(install)?;
    let client = login_client()?;

    let email_hex = hex_encode(email);
    let password_hex = hex_encode(password);
    let request = LoginRequest {
        email: &email_hex,
        password: &password_hex,
        code: otp.unwrap_or(""),
        token: recaptcha_token,
    };
    let (status, response) =
        post_api::<LoginResponse>(&client, LOGIN_URL, &request, "繁中服 launcherLogin").await?;
    let login_token = accepted_value(status, response.error, response.token, "繁中服 launcherLogin")?;

    // 伺服器要求取 session 前先送版本檢查；需更新時不取 session。
    if let VersionCheck::PatchList(list) = check_version_with(&client, &game_ver, version_body).await? {
        return Ok(VersionCheck::PatchList(list));
    }

    let request = SessionRequest {
        token: &login_token,
    };
    let (status, response) =
        post_api::<SessionResponse>(&client, SESSION_URL, &request, "繁中服 launcherSession")
            .await?;
    let session_id = accepted_value(
        status,
        response.error,
        response.session_id,
        "繁中服 launcherSession",
    )?;
    // 啟動參數以空白分隔，含空白或控制字元的 sessionId 會改寫其他參數。
    if !session_id.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(AppError::protocol("繁中服 launcherSession（sessionId 格式）"));
    }
    Ok(VersionCheck::UpToDate(TwSession { session_id }))
}

#[derive(Serialize)]
struct LoginRequest<'a> {
    email: &'a str,
    password: &'a str,
    code: &'a str,
    token: &'a str,
}

#[derive(Deserialize)]
struct LoginResponse {
    token: Option<String>,
    error: Option<String>,
}

#[derive(Serialize)]
struct SessionRequest<'a> {
    token: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionResponse {
    session_id: Option<String>,
    error: Option<String>,
}

// 不設 default headers：登入 API 與 patch 伺服器的 header 組合不同（§2.1、§2.3）。
// reqwest 未啟用 cookies feature，不保存 cookie。
fn http_client() -> AppResult<Client> {
    Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| AppError::network("建立 HTTP client", e))
}

/// 登入流程的 client，連線特徵對齊 TC（.NET HttpClient）：只走 HTTP/1.1（ALPN 不提 h2）、header 名稱 Title-Case、
/// 不送 Accept-Encoding（不開 gzip／deflate 解壓）。只用於 [`login`]；獨立的版本檢查與 patch 下載不用。
fn login_client() -> AppResult<Client> {
    login_client_with(Client::builder())
}

fn login_client_with(builder: ClientBuilder) -> AppResult<Client> {
    builder
        .timeout(HTTP_TIMEOUT)
        .http1_only()
        .http1_title_case_headers()
        .no_gzip()
        .no_deflate()
        .no_brotli()
        .no_zstd()
        .build()
        .map_err(|e| AppError::network("建立 HTTP client", e))
}

/// POST JSON 到台服登入 API；回應必須是 JSON 物件，否則視為格式不符（不回傳原文）。
async fn post_api<T: DeserializeOwned>(
    client: &Client,
    url: &str,
    body: &impl Serialize,
    context: &'static str,
) -> AppResult<(u16, T)> {
    let response = client
        .post(url)
        .header(USER_AGENT, TW_USER_AGENT)
        .header(ACCEPT_LANGUAGE, TW_ACCEPT_LANGUAGE)
        .header(ACCEPT, TW_ACCEPT)
        // `.json()` 只在沒有 Content-Type 時補 `application/json`，這裡先放帶 charset 的。
        .header(CONTENT_TYPE, TW_JSON_CONTENT_TYPE)
        .json(body)
        .send()
        .await
        .map_err(|e| AppError::network(context, e))?;
    let status = response.status().as_u16();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| AppError::network(context, e))?;
    let parsed = serde_json::from_slice::<T>(&bytes)
        .map_err(|_| AppError::protocol(format!("{context}（HTTP {status}）")))?;
    Ok((status, parsed))
}

/// 伺服器 `error` 優先轉成登入被拒；否則要 2xx 且帶非空值，其餘都是格式不符。
fn accepted_value(
    status: u16,
    error: Option<String>,
    value: Option<String>,
    context: &str,
) -> AppResult<String> {
    if let Some(message) = error.filter(|m| !m.trim().is_empty()) {
        return Err(AppError::LoginRejected {
            region: Region::Tw,
            message,
        });
    }
    match value {
        Some(value) if is_success(status) && !value.is_empty() => Ok(value),
        Some(_) | None => Err(AppError::protocol(format!("{context}（HTTP {status}）"))),
    }
}

/// 不需登入的版本檢查（protocols §2.3），供啟動前與「檢查更新」使用。`ffxivgame.ver` 缺失時不送請求。
pub async fn check_version(install: &Path) -> AppResult<VersionCheck<()>> {
    let (game_ver, body) = version_check_inputs(install)?;
    check_version_with(&http_client()?, &game_ver, body).await
}

async fn check_version_with(
    client: &Client,
    game_ver: &str,
    body: String,
) -> AppResult<VersionCheck<()>> {
    let response = client
        .post(format!("{VERSION_CHECK_URL}{game_ver}/"))
        .header("X-Hash-Check", "enabled")
        .body(body)
        .send()
        .await
        .map_err(|e| AppError::network("繁中服版本檢查", e))?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_default();
    // 只有 2xx 且非 204 的 body 影響判定，其他狀態不讀 body。
    let body = if status != 204 && is_success(status) {
        response
            .bytes()
            .await
            .map_err(|e| AppError::network("繁中服版本檢查", e))?
            .to_vec()
    } else {
        Vec::new()
    };
    classify_version_check(status, content_type, body)
}

/// 版本檢查的 URL 版本與 body。`ffxivgame.ver` 缺失即 [`AppError::VersionFileMissing`]（不以基礎版本兜底
/// 要清單，主 note「版本來源」）；body 開頭的 `\n` 代表不送 boot 版本，之後只列存在的資料片（§2.3）。
fn version_check_inputs(install: &Path) -> AppResult<(String, String)> {
    let game_ver = update::required_game_version(Region::Tw, install)?;
    let mut body = String::from("\n");
    for n in 1..=game::MAX_EXPANSION {
        if let Some(ver) = game::expansion_version(install, n)? {
            body.push_str(&format!("ex{n}\t{ver}\n"));
        }
    }
    Ok((game_ver, body))
}

/// 判定順序固定：204 → 非 2xx → 2xx 是否含 patch URL（protocols §2.3）。含 patch URL 時交回
/// Content-Type 與 body，由呼叫端以 `patch::list::parse_patch_list` 解析。
fn classify_version_check(
    status: u16,
    content_type: String,
    body: Vec<u8>,
) -> AppResult<VersionCheck<()>> {
    if status == 204 {
        return Ok(VersionCheck::UpToDate(()));
    }
    if !is_success(status) {
        return Err(AppError::VersionCheckFailed {
            region: Region::Tw,
            status,
        });
    }
    let has_patch_url = PATCH_URL_MARKERS.iter().any(|marker| {
        let marker = marker.as_bytes();
        body.windows(marker.len()).any(|w| w == marker)
    });
    if has_patch_url {
        Ok(VersionCheck::PatchList(RawPatchList { content_type, body }))
    } else {
        Ok(VersionCheck::UpToDate(()))
    }
}

fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};

    use super::*;

    #[test]
    fn hex_encode_is_lowercase_utf8_hex() {
        assert_eq!(hex_encode(""), "");
        assert_eq!(hex_encode("Ab@1"), "41624031");
        assert_eq!(hex_encode("user@example.com"), "75736572406578616d706c652e636f6d");
        // 非 ASCII 依 UTF-8 bytes 編碼。
        assert_eq!(hex_encode("台服"), "e58fb0e69c8d");
    }

    fn classify(status: u16, body: &[u8]) -> AppResult<VersionCheck<()>> {
        classify_version_check(status, String::new(), body.to_vec())
    }

    #[test]
    fn version_check_204_needs_no_update() {
        assert!(matches!(classify(204, b""), Ok(VersionCheck::UpToDate(()))));
        // 204 優先於 body 內容。
        assert!(matches!(
            classify(204, PATCH_URL_MARKERS[0].as_bytes()),
            Ok(VersionCheck::UpToDate(()))
        ));
    }

    #[test]
    fn version_check_non_2xx_fails_with_status() {
        assert!(matches!(
            classify(500, b""),
            Err(AppError::VersionCheckFailed {
                region: Region::Tw,
                status: 500
            })
        ));
    }

    /// 台服實際回應（2026-10-13，HTTP 200，同 `patch::list` 測試素材）。
    const TW_BODY: &[u8] = include_bytes!("../patch/testdata/tw_patchlist_2026-10-13.bin");
    const TW_CONTENT_TYPE: &str = include_str!("../patch/testdata/tw_patchlist_2026-10-13.ct");

    #[test]
    fn version_check_2xx_with_patch_url_hands_back_list() {
        let result =
            classify_version_check(200, TW_CONTENT_TYPE.trim().to_string(), TW_BODY.to_vec())
                .expect("list");
        let VersionCheck::PatchList(list) = result else {
            panic!("應交回清單");
        };
        assert_eq!(list.content_type, TW_CONTENT_TYPE.trim());
        assert_eq!(list.body, TW_BODY);
        let entries =
            crate::patch::list::parse_patch_list(Region::Tw, &list.content_type, &list.body)
                .expect("parse");
        assert_eq!(entries.len(), 9);
    }

    /// 清單 URL 改成 https 時照樣判定需要更新並交回清單（解析後下載位址同樣是 https）。
    #[test]
    fn version_check_2xx_with_https_patch_url_hands_back_list() {
        let body = std::str::from_utf8(TW_BODY)
            .expect("utf8")
            .replace("http://patch-dl.ffxiv.com.tw/", "https://patch-dl.ffxiv.com.tw/");
        let result =
            classify_version_check(200, TW_CONTENT_TYPE.trim().to_string(), body.clone().into_bytes())
                .expect("list");
        let VersionCheck::PatchList(list) = result else {
            panic!("應交回清單");
        };
        assert_eq!(list.body, body.as_bytes());
        let entries =
            crate::patch::list::parse_patch_list(Region::Tw, &list.content_type, &list.body)
                .expect("parse");
        assert_eq!(entries.len(), 9);
        assert!(entries.iter().all(|e| e.url.scheme() == "https"));
    }

    #[test]
    fn version_check_2xx_without_patch_url_needs_no_update() {
        assert!(matches!(classify(200, b""), Ok(VersionCheck::UpToDate(()))));
        assert!(matches!(
            classify(200, b"--477D80B1_2D6B_4E3A\r\n\r\n"),
            Ok(VersionCheck::UpToDate(()))
        ));
    }

    #[tokio::test]
    async fn missing_game_version_is_rejected_before_any_request() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 先檢查 ffxivgame.ver 才建 client；送了請求會是網路或登入錯誤，不是這個。
        let err = check_version(dir.path()).await.expect_err("err");
        assert!(
            matches!(err, AppError::VersionFileMissing { region: Region::Tw, .. }),
            "{err}"
        );
        let err = login(dir.path(), "user@example.com", "password", None, "token")
            .await
            .expect_err("err");
        assert!(
            matches!(err, AppError::VersionFileMissing { region: Region::Tw, .. }),
            "{err}"
        );
    }

    #[test]
    fn launch_args_match_protocol() {
        let session = TwSession {
            session_id: "abc123".to_string(),
        };
        // 逐字取自 protocols §2.2，不用續行字串，避免與實作犯同一個錯。
        let expected = "DEV.LobbyHost01=neolobby01.ffxiv.com.tw DEV.LobbyPort01=54994 DEV.GMServerHost=frontier.ffxiv.com.tw DEV.TestSID=abc123 SYS.resetConfig=0 DEV.SaveDataBankHost=config-dl.ffxiv.com.tw";
        assert_eq!(launch_args(&session, &[]), expected);
        let extra = [("SYS.ScreenMode".to_string(), "2".to_string())];
        assert_eq!(launch_args(&session, &extra), format!("{expected} SYS.ScreenMode=2"));
    }

    type RequestHead = (String, Vec<(String, String)>);

    /// 收一個請求，記下 request line 與 header（名稱轉小寫），讀完 body 後回 200 `{"token":"t"}`。
    fn serve_one_json_request() -> (String, std::thread::JoinHandle<RequestHead>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/api/login/launcherLogin", listener.local_addr().expect("addr"));
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut request_line = String::new();
            reader.read_line(&mut request_line).expect("request line");
            let mut headers = Vec::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("header line");
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                let (name, value) = line.split_once(':').expect("header");
                headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
            }
            let length = headers
                .iter()
                .find(|(name, _)| name == "content-length")
                .map_or(0, |(_, value)| value.parse::<usize>().expect("content-length"));
            reader.read_exact(&mut vec![0; length]).expect("body");
            let body = br#"{"token":"t"}"#;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(head.as_bytes())
                .and_then(|()| stream.write_all(body))
                .expect("respond");
            (request_line.trim_end().to_string(), headers)
        });
        (url, server)
    }

    /// 登入 client 對齊 TC：HTTP/1.1、不送 Accept-Encoding、Content-Type 帶 charset，其餘 header 照 §2.1。
    #[tokio::test]
    async fn login_requests_match_tc_headers_over_http1() {
        let (url, server) = serve_one_json_request();
        let client = login_client_with(Client::builder().no_proxy()).expect("client");
        let request = SessionRequest { token: "t" };
        let (status, response) = post_api::<LoginResponse>(&client, &url, &request, "測試").await.expect("post");
        assert_eq!(status, 200);
        assert_eq!(response.token.as_deref(), Some("t"));

        let (request_line, headers) = server.join().expect("server");
        assert!(request_line.ends_with(" HTTP/1.1"), "{request_line}");
        let values = |name: &str| -> Vec<String> {
            headers.iter().filter(|(n, _)| n == name).map(|(_, v)| v.clone()).collect()
        };
        assert_eq!(values("content-type"), [TW_JSON_CONTENT_TYPE]);
        assert!(values("accept-encoding").is_empty(), "{:?}", values("accept-encoding"));
        assert_eq!(values("user-agent"), [TW_USER_AGENT]);
        assert_eq!(values("accept-language"), [TW_ACCEPT_LANGUAGE]);
        assert_eq!(values("accept"), [TW_ACCEPT]);
    }

    /// 讀用戶端的第一個 TLS record（ClientHello），回傳 ALPN 擴充列出的協定；不回應，握手不會完成。
    fn client_hello_alpn(stream: &mut TcpStream) -> Vec<String> {
        let mut header = [0u8; 5];
        stream.read_exact(&mut header).expect("record header");
        assert_eq!(header[0], 0x16, "handshake record");
        let mut hello = vec![0; usize::from(u16::from_be_bytes([header[3], header[4]]))];
        stream.read_exact(&mut hello).expect("record");
        assert_eq!(hello[0], 0x01, "client_hello");
        let u16_at = |at: usize| usize::from(u16::from_be_bytes([hello[at], hello[at + 1]]));
        // handshake header 4 + legacy_version 2 + random 32，接著 session_id、cipher_suites、compression。
        let mut at = 4 + 2 + 32;
        at += 1 + usize::from(hello[at]);
        at += 2 + u16_at(at);
        at += 1 + usize::from(hello[at]);
        let end = at + 2 + u16_at(at);
        at += 2;
        while at + 4 <= end {
            let (kind, len) = (u16_at(at), u16_at(at + 2));
            let data = &hello[at + 4..at + 4 + len];
            if kind == 0x0010 {
                // ALPN：2 bytes 清單長度，之後每項 1 byte 長度＋名稱。
                let mut names = Vec::new();
                let mut i = 2;
                while i < data.len() {
                    let n = usize::from(data[i]);
                    names.push(String::from_utf8_lossy(&data[i + 1..i + 1 + n]).into_owned());
                    i += 1 + n;
                }
                return names;
            }
            at += 4 + len;
        }
        Vec::new()
    }

    /// 用 `client` 對本機 TLS 位址發一個請求，回傳 ClientHello 裡的 ALPN。
    async fn offered_alpn(client: &Client) -> Vec<String> {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("https://{}/", listener.local_addr().expect("addr"));
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            client_hello_alpn(&mut stream)
        });
        assert!(client.post(&url).send().await.is_err(), "伺服器只讀 ClientHello，握手不該完成");
        server.join().expect("server")
    }

    /// 登入 client 的 ALPN 只提 http/1.1，HTTPS 伺服器就選不到 HTTP/2；預設 client（開了 http2 feature）會提 h2。
    #[tokio::test]
    async fn login_client_offers_only_http1_over_tls() {
        let login = login_client_with(Client::builder().no_proxy()).expect("client");
        assert_eq!(offered_alpn(&login).await, ["http/1.1"]);
        let default = Client::builder().no_proxy().build().expect("client");
        assert!(offered_alpn(&default).await.contains(&"h2".to_string()));
    }
}
