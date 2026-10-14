//! 國際服 Lease、boot 版本檢查、OAuth 登入與 session 註冊（protocols §1.1、§1.3、§1.4）。
//! session 註冊回 patch list 時把 Content-Type 與 body 交回呼叫端（§5.1），UID 不隨清單交出。
//!
//! 不變量：密碼、OTP、OAuth SID、UID 與任何回應原文都不進錯誤或 log；URL 含 SID，
//! 網路錯誤一律經 `AppError::network` 去掉 URL。

use std::fmt;
use std::fs;
use std::path::Path;
use std::time::Duration;

use regex::Regex;
use reqwest::header::{
    ACCEPT, ACCEPT_LANGUAGE, CACHE_CONTROL, CONNECTION, CONTENT_TYPE, COOKIE, REFERER, USER_AGENT,
};
use serde::Deserialize;
use sha1::{Digest, Sha1};

use crate::error::{AppError, AppResult};
use crate::game;
use crate::patch::update::{self, RawPatchList, VersionCheck};
use crate::region::Region;

const LEASE_URL: &str = "https://kamori.goats.dev/Launcher/GetLease";
const OAUTH_TOP_URL: &str = "https://ffxiv-login.square-enix.com/oauth/ffxivarr/login/top?lng=en&rgn=3&isft=0&cssmode=1&isnew=1&launchver=3";
const LOGIN_SEND_URL: &str = "https://ffxiv-login.square-enix.com/oauth/ffxivarr/login/login.send";
const PATCH_USER_AGENT: &str = "FFXIV PATCH CLIENT";
/// 官方啟動器（IE 元件）送出的 Accept。
const LAUNCHER_ACCEPT: &str = "image/gif, image/jpeg, image/pjpeg, application/x-ms-application, application/xaml+xml, application/x-ms-xbap, */*";
/// XL 首次啟動時以 `GenerateAcceptLanguage()`（固定 seed 0 的 .NET Random）產生並保存；
/// 2026-10-12 以 dotnet 重現該演算法，結果為 "ja"。
const ACCEPT_LANGUAGE_VALUE: &str = "ja";
/// XL 在 frontier Referer 用 `GetLangCode().Replace("-", "_")`，英文為 "en_gb"。
const FRONTIER_LANG: &str = "en_gb";
/// Session 註冊時要回報雜湊的 boot 檔（依此順序）。
const BOOT_FILES: [&str; 4] = [
    "ffxivboot.exe",
    "ffxivboot64.exe",
    "ffxivlauncher64.exe",
    "ffxivupdater64.exe",
];
const LOGIN_REJECTED_MESSAGE: &str = "帳號、密碼或一次性密碼錯誤，或伺服器拒絕登入";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Lease `Flags` 的 GlobalDisableDalamud 位元。
const FLAG_GLOBAL_DISABLE_DALAMUD: u32 = 1;
const FLAG_FORCE_PROXY_DALAMUD_AND_ASSETS: u32 = 1 << 1;

/// goatcorp 的 Lease：frontier URL 範本與遠端功能旗標。
#[derive(Debug, Clone)]
pub struct Lease {
    pub frontier_url: String,
    pub flags: u32,
}

impl Lease {
    /// goatcorp 遠端停用國際服衛月（`Flags & 1`）；為真時不得注入。
    pub fn dalamud_disabled(&self) -> bool {
        self.flags & FLAG_GLOBAL_DISABLE_DALAMUD != 0
    }
}

// 服務端實際回 camelCase（2026-10-12 實測），XL 的 Newtonsoft 不分大小寫；兩種都接。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeaseResponse {
    #[serde(alias = "Success")]
    success: bool,
    #[serde(alias = "FrontierUrl", default)]
    frontier_url: Option<String>,
    #[serde(alias = "Flags", default)]
    flags: Option<serde_json::Value>,
}

/// 取得 Lease。非 2xx、body 不是預期 JSON、`Success` 為假或缺 FrontierUrl 一律
/// `DalamudLeaseUnavailable`（呼叫端據此不注入國際服衛月，fail closed）。
///
/// 2026-10-12 實測：服務端只檢查 `X-XL-Track` 與 `X-XL-LV`（缺任一回空 body 或
/// "Unknown or invalid track."），不檢查 User-Agent 與 `X-XL-HaveVersion`，
/// 故以本專案自己的 User-Agent 請求，只送這兩個必要 header。
pub async fn fetch_lease() -> AppResult<Lease> {
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| AppError::network("建立 HTTP client", e))?;
    let resp = client
        .get(LEASE_URL)
        .header(
            USER_AGENT,
            concat!("xiv-launch-manager/", env!("CARGO_PKG_VERSION")),
        )
        .header("X-XL-Track", "Release")
        .header("X-XL-LV", "0")
        .send()
        .await
        .map_err(|e| AppError::network("取得 Lease", e))?;
    if !resp.status().is_success() {
        return Err(AppError::DalamudLeaseUnavailable);
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| AppError::network("讀取 Lease", e))?;
    parse_lease(&body)
}

fn parse_lease(body: &[u8]) -> AppResult<Lease> {
    let data: LeaseResponse =
        serde_json::from_slice(body).map_err(|_| AppError::DalamudLeaseUnavailable)?;
    if !data.success {
        return Err(AppError::DalamudLeaseUnavailable);
    }
    let frontier_url = data
        .frontier_url
        .filter(|u| !u.is_empty())
        .ok_or(AppError::DalamudLeaseUnavailable)?;
    let flags = match data.flags {
        None | Some(serde_json::Value::Null) => 0,
        Some(value) => parse_lease_flags(&value).ok_or(AppError::DalamudLeaseUnavailable)?,
    };
    Ok(Lease {
        frontier_url,
        flags,
    })
}

/// Flags 可能是數字，或 .NET enum 的名稱字串（"GlobalDisableDalamud, ..."）。
/// 無法辨識時回 `None`，由呼叫端視為取得失敗（不能確認是否被停用）。
fn parse_lease_flags(value: &serde_json::Value) -> Option<u32> {
    match value {
        serde_json::Value::Number(n) => n.as_u64().and_then(|v| u32::try_from(v).ok()),
        serde_json::Value::String(s) => {
            let mut flags = 0;
            for name in s.split(',').map(str::trim) {
                flags |= match name {
                    "None" => 0,
                    "GlobalDisableDalamud" => FLAG_GLOBAL_DISABLE_DALAMUD,
                    "ForceProxyDalamudAndAssets" => FLAG_FORCE_PROXY_DALAMUD_AND_ASSETS,
                    other => other.parse::<u32>().ok()?,
                };
            }
            Some(flags)
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Array(_)
        | serde_json::Value::Object(_) => None,
    }
}

/// 官方啟動器的 computerId：SHA1(UTF-16LE(machine+user+os+cpus)) 前 4 bytes，
/// 前置 1 byte checksum `-(b1+b2+b3+b4)`，共 10 個小寫 hex 字元。
pub fn computer_id(machine: &str, user: &str, os: &str, cpus: u32) -> String {
    let text = format!("{machine}{user}{os}{cpus}");
    let utf16: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let hash = Sha1::digest(&utf16);
    let mut bytes = [0u8; 5];
    bytes[1..].copy_from_slice(&hash[..4]);
    bytes[0] = bytes[1..]
        .iter()
        .fold(0u8, |acc, b| acc.wrapping_add(*b))
        .wrapping_neg();
    hex::encode(bytes)
}

/// 本機的 computerId，輸入對應 .NET 的 `Environment.MachineName`、`UserName`、
/// `OSVersion`（"Microsoft Windows NT {major}.{minor}.{build}.0"）與 `ProcessorCount`。
///
/// computerId 只是 SE 用來辨識裝置的穩定值，不參與驗證；某項取不到時以空字串／0
/// 代入，結果仍穩定，不必讓登入失敗。
pub fn local_computer_id() -> String {
    // quality-allow: computerId 只是裝置識別值、不參與驗證，取不到的部分以空值代入仍穩定
    let machine = std::env::var("COMPUTERNAME").unwrap_or_default();
    // quality-allow: 同上，computerId 輸入缺項時以空值代入
    let user = std::env::var("USERNAME").unwrap_or_default();
    // quality-allow: 同上，computerId 輸入缺項時以空值代入
    let os = local_os_version().unwrap_or_default();
    let cpus = std::thread::available_parallelism()
        .ok()
        .and_then(|n| u32::try_from(n.get()).ok())
        .unwrap_or(0); // quality-allow: 同上，computerId 輸入缺項時以 0 代入
    computer_id(&machine, &user, &os, cpus)
}

fn local_os_version() -> Option<String> {
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;
    // 登錄檔的版本號不受應用程式相容性 manifest 影響（GetVersionEx 會被降成 6.2）。
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
        .ok()?;
    let major: u32 = key.get_value("CurrentMajorVersionNumber").ok()?;
    let minor: u32 = key.get_value("CurrentMinorVersionNumber").ok()?;
    let build: String = key.get_value("CurrentBuildNumber").ok()?;
    Some(format!("Microsoft Windows NT {major}.{minor}.{build}.0"))
}

/// OAuth 頁面用的 User-Agent（protocols §1.3）。
pub fn user_agent() -> String {
    format!(
        "SQEXAuthor/2.0.0(Windows 6.2; ja-jp; {})",
        local_computer_id()
    )
}

/// 國際服 boot 版本檢查（protocols §1.1 步驟 1）。
pub async fn check_boot(install: &Path) -> AppResult<()> {
    let boot_version = game::boot_version(install)?;
    let url = format!(
        "http://patch-bootver.ffxiv.com/http/win32/ffxivneo_release_boot/{boot_version}/?time={}",
        rounded_patch_time()
    );
    // Host 由 reqwest 依 URL 自動帶入，值即 patch-bootver.ffxiv.com。
    let resp = se_client()?
        .get(url)
        .header(USER_AGENT, PATCH_USER_AGENT)
        .send()
        .await
        .map_err(|e| AppError::network("國際服 boot 版本檢查", e))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(AppError::VersionCheckFailed {
            region: Region::Global,
            status: status.as_u16(),
        });
    }
    let text = resp
        .text()
        .await
        .map_err(|e| AppError::network("讀取 boot 版本檢查回應", e))?;
    if text.trim().is_empty() {
        Ok(())
    } else {
        Err(AppError::NeedsUpdate {
            region: Region::Global,
        })
    }
}

/// 登入成功後可用於啟動的 session。`unique_id` 是 session 註冊回應的
/// `X-Patch-Unique-Id`（啟動參數 `DEV.TestSID`），屬 secret。
#[derive(Clone)]
pub struct GlobalSession {
    pub unique_id: String,
    pub region: u32,
    pub max_expansion: u8,
}

impl fmt::Debug for GlobalSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GlobalSession")
            .field("unique_id", &"<redacted>")
            .field("region", &self.region)
            .field("max_expansion", &self.max_expansion)
            .finish()
    }
}

/// OAuth 登入結果。`sid` 只用來註冊 session，不得外流；故不實作 Debug。
struct OauthResult {
    sid: String,
    terms_accepted: bool,
    region: u32,
    playable: bool,
    max_expansion: u8,
}

/// 國際服登入（protocols §1.1 步驟 2–6）。呼叫前應先 `check_boot`。session 註冊回 patch list 時
/// 回 [`VersionCheck::PatchList`]（不含 UID，更新後要重新登入）。`ffxivgame.ver` 缺失時在送出任何請求前
/// 就回 [`AppError::VersionFileMissing`]（不以基礎版本兜底要清單）。
pub async fn login(
    lease: &Lease,
    install: &Path,
    username: &str,
    password: &str,
    otp: Option<&str>,
) -> AppResult<VersionCheck<GlobalSession>> {
    let game_version = update::required_game_version(Region::Global, install)?;
    let client = se_client()?;
    let user_agent = user_agent();

    let top = client
        .get(OAUTH_TOP_URL)
        .header(ACCEPT, LAUNCHER_ACCEPT)
        .header(REFERER, frontier_referer(&lease.frontier_url))
        .header(ACCEPT_LANGUAGE, ACCEPT_LANGUAGE_VALUE)
        .header(USER_AGENT, &user_agent)
        .header(CONNECTION, "Keep-Alive")
        .header(COOKIE, "_rsid=\"\"")
        .send()
        .await
        .map_err(|e| AppError::network("國際服登入頁", e))?
        .text()
        .await
        .map_err(|e| AppError::network("讀取國際服登入頁", e))?;
    let stored = parse_stored(&top)?;

    let form = [
        ("_STORED_", stored.as_str()),
        ("sqexid", username),
        ("password", password),
        ("otppw", otp.unwrap_or("")),
    ];
    let reply = client
        .post(LOGIN_SEND_URL)
        .header(ACCEPT, LAUNCHER_ACCEPT)
        .header(REFERER, OAUTH_TOP_URL)
        .header(ACCEPT_LANGUAGE, ACCEPT_LANGUAGE_VALUE)
        .header(USER_AGENT, &user_agent)
        .header(CONNECTION, "Keep-Alive")
        .header(CACHE_CONTROL, "no-cache")
        .header(COOKIE, "_rsid=\"\"")
        .form(&form)
        .send()
        .await
        .map_err(|e| AppError::network("國際服登入", e))?
        .text()
        .await
        .map_err(|e| AppError::network("讀取國際服登入回應", e))?;
    let oauth = parse_oauth_reply(&reply)?;

    // 檢查順序同官方：先可遊玩，再條款。
    if !oauth.playable {
        return Err(AppError::NotPlayable);
    }
    if !oauth.terms_accepted {
        return Err(AppError::TermsNotAccepted);
    }

    let unique_id = match register_session(&client, install, &game_version, &oauth).await? {
        VersionCheck::UpToDate(unique_id) => unique_id,
        VersionCheck::PatchList(list) => return Ok(VersionCheck::PatchList(list)),
    };
    Ok(VersionCheck::UpToDate(GlobalSession {
        unique_id,
        region: oauth.region,
        max_expansion: oauth.max_expansion,
    }))
}

/// 回 `UpToDate(UID)` 或 patch list（判定見 [`classify_session`]）。
async fn register_session(
    client: &reqwest::Client,
    install: &Path,
    game_version: &str,
    oauth: &OauthResult,
) -> AppResult<VersionCheck<String>> {
    let report = version_report(install, oauth.max_expansion)?;
    let url = format!(
        "https://patch-gamever.ffxiv.com/http/win32/ffxivneo_release_game/{game_version}/{}",
        oauth.sid
    );
    let resp = client
        .post(url)
        .header(USER_AGENT, PATCH_USER_AGENT)
        .header("X-Hash-Check", "enabled")
        .header(CONNECTION, "Keep-Alive")
        .body(report)
        .send()
        .await
        .map_err(|e| AppError::network("國際服 session 註冊", e))?;

    let status = resp.status().as_u16();
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let unique_id = header("X-Patch-Unique-Id");
    let content_type = header(CONTENT_TYPE.as_str()).unwrap_or_default();
    // 只有 2xx 的 body 影響判定；409／410 等不讀 body，讀取失敗不會蓋掉狀態碼的意義。
    let body = if (200..300).contains(&status) {
        resp.bytes()
            .await
            .map_err(|e| AppError::network("讀取國際服 session 註冊回應", e))?
            .to_vec()
    } else {
        Vec::new()
    };
    classify_session(status, unique_id, content_type, body)
}

/// protocols §5.1：409＝boot 需修復；410＝版本不再服務（不是清單來源，提示官方啟動器）；其他非 2xx＝檢查失敗；
/// 2xx 必須有 UID；空 body＝無需更新（回 UID）；非空 body＝patch list（只交回 Content-Type 與 body）。
fn classify_session(
    status: u16,
    unique_id: Option<String>,
    content_type: String,
    body: Vec<u8>,
) -> AppResult<VersionCheck<String>> {
    match status {
        409 => return Err(AppError::BootModified),
        410 => {
            return Err(AppError::NeedsUpdate {
                region: Region::Global,
            });
        }
        code if !(200..300).contains(&code) => {
            return Err(AppError::VersionCheckFailed {
                region: Region::Global,
                status: code,
            });
        }
        _ => {}
    }
    let unique_id = unique_id
        .filter(|v| !v.is_empty())
        .ok_or_else(|| AppError::protocol("國際服 session 註冊缺少 X-Patch-Unique-Id"))?;
    if body.is_empty() {
        Ok(VersionCheck::UpToDate(unique_id))
    } else {
        Ok(VersionCheck::PatchList(RawPatchList { content_type, body }))
    }
}

/// SE 端點用的 client：HTTP/1.1（請求帶 `Connection: Keep-Alive`，h2 不允許）、
/// gzip／deflate 由 reqwest 自動帶 `Accept-Encoding: gzip, deflate` 並解壓、
/// 不保存 cookie（未啟用 cookie store）、hyper 不送 `Expect: 100-continue`。
fn se_client() -> AppResult<reqwest::Client> {
    reqwest::Client::builder()
        .http1_only()
        .gzip(true)
        .deflate(true)
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| AppError::network("建立 HTTP client", e))
}

/// frontier Referer：Lease 範本 `{0}`＝語言碼、`{1}`＝UTC yyyy-MM-dd-HH-mm。
fn frontier_referer(template: &str) -> String {
    let time = chrono::Utc::now().format("%Y-%m-%d-%H-%M").to_string();
    template.replace("{0}", FRONTIER_LANG).replace("{1}", &time)
}

/// UTC yyyy-MM-dd-HH-mm，分鐘個位數歸 0（boot 版本檢查用）。
fn rounded_patch_time() -> String {
    let now = chrono::Utc::now();
    format!(
        "{}{}0",
        now.format("%Y-%m-%d-%H-"),
        chrono::Timelike::minute(&now) / 10
    )
}

/// 從 OAuth top 頁取 `_STORED_`。
fn parse_stored(page: &str) -> AppResult<String> {
    let re = Regex::new(r#"\t<\s*input .* name="_STORED_" value="(?<stored>.*)">"#)
        .map_err(|_| AppError::protocol("國際服登入頁解析"))?;
    re.captures(page)
        .and_then(|c| c.name("stored"))
        .map(|m| m.as_str().to_owned())
        .ok_or_else(|| AppError::protocol("國際服登入頁缺少 _STORED_"))
}

/// 解析 login.send 回應。不符成功格式＝登入被拒（回應原文可能含 SID，不帶出）。
fn parse_oauth_reply(reply: &str) -> AppResult<OauthResult> {
    let re = Regex::new(r#"window.external.user\("login=auth,ok,(?<launchParams>.*)\);"#)
        .map_err(|_| AppError::protocol("國際服登入回應解析"))?;
    let params = re
        .captures(reply)
        .and_then(|c| c.name("launchParams"))
        .ok_or_else(|| AppError::LoginRejected {
            region: Region::Global,
            message: LOGIN_REJECTED_MESSAGE.to_string(),
        })?
        .as_str();

    let fields: Vec<&str> = params.split(',').collect();
    let field = |i: usize| {
        fields
            .get(i)
            .copied()
            .ok_or_else(|| AppError::protocol("國際服登入回應欄位不足"))
    };
    let sid = field(1)?;
    if sid.is_empty() {
        return Err(AppError::protocol("國際服登入回應缺少 SID"));
    }
    Ok(OauthResult {
        sid: sid.to_owned(),
        terms_accepted: field(3)? != "0",
        region: field(5)?
            .parse()
            .map_err(|_| AppError::protocol("國際服登入回應 region 格式"))?,
        playable: field(9)? != "0",
        max_expansion: field(13)?
            .parse()
            .map_err(|_| AppError::protocol("國際服登入回應資料片格式"))?,
    })
}

/// Session 註冊 body：boot 檔雜湊行，接著每個 ex{1..=max} 一行版本。`exN.ver` 缺失即
/// [`AppError::VersionFileMissing`]，不以基礎版本兜底（主 note「版本來源」）。
fn version_report(install: &Path, max_expansion: u8) -> AppResult<String> {
    let boot_dir = install.join("boot");
    let mut report = game::boot_version(install)?;
    report.push('=');
    for (i, name) in BOOT_FILES.iter().enumerate() {
        let path = boot_dir.join(name);
        let bytes = fs::read(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => AppError::GameFileMissing {
                path: path.display().to_string(),
            },
            _ => AppError::io(format!("讀取 {}", path.display()), e),
        })?;
        if i > 0 {
            report.push(',');
        }
        report.push_str(&format!(
            "{name}/{}/{}",
            bytes.len(),
            hex::encode(Sha1::digest(&bytes))
        ));
    }
    report.push('\n');
    for n in 1..=max_expansion {
        let version = update::required_expansion_version(Region::Global, install, n)?;
        report.push_str(&format!("ex{n}\t{version}\n"));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computer_id_matches_xl() {
        // 期望值由 XL MakeComputerId 演算法以 dotnet 產生（xiv-oracle Program.cs `extra`）。
        assert_eq!(
            computer_id(
                "DESKTOP-ABC123",
                "shawn",
                "Microsoft Windows NT 10.0.26300.0",
                16
            ),
            "6f4290cff0"
        );
        assert_eq!(
            computer_id("PC", "\u{6797}", "Microsoft Windows NT 6.2.9200.0", 4),
            "0c855deb27"
        );
    }

    #[test]
    fn stored_is_extracted() {
        let page = "<form>\n\t<input type=\"hidden\" name=\"_STORED_\" value=\"abc123DEF\">\n</form>";
        assert_eq!(parse_stored(page).expect("stored"), "abc123DEF");
        assert!(matches!(
            parse_stored("<html></html>"),
            Err(AppError::Protocol { .. })
        ));
    }

    #[test]
    fn oauth_success_is_parsed() {
        let reply = "<script>window.external.user(\"login=auth,ok,sid,SECRETSID0123,terms,1,region,3,etmadd,0,playable,1,ps3pkg,0,maxex,5,product,1\");</script>";
        let r = parse_oauth_reply(reply).expect("ok");
        assert_eq!(r.sid, "SECRETSID0123");
        assert!(r.terms_accepted);
        assert_eq!(r.region, 3);
        assert!(r.playable);
        assert_eq!(r.max_expansion, 5);

        let reply = "window.external.user(\"login=auth,ok,sid,X,terms,0,region,1,etmadd,0,playable,0,ps3pkg,0,maxex,2,product,1\");";
        let r = parse_oauth_reply(reply).expect("ok");
        assert!(!r.terms_accepted);
        assert!(!r.playable);
        assert_eq!(r.region, 1);
        assert_eq!(r.max_expansion, 2);
    }

    #[test]
    fn oauth_failure_is_classified_without_raw_text() {
        let reply = "window.external.user(\"login=auth,ng,err,SECRETSID0123 bad password\");";
        let err = parse_oauth_reply(reply).err().expect("err");
        assert!(matches!(
            err,
            AppError::LoginRejected {
                region: Region::Global,
                ..
            }
        ));
        assert!(!err.to_string().contains("SECRETSID0123"));

        let truncated = "window.external.user(\"login=auth,ok,sid,SECRETSID0123,terms,1\");";
        let err = parse_oauth_reply(truncated).err().expect("err");
        assert!(matches!(err, AppError::Protocol { .. }));
        assert!(!err.to_string().contains("SECRETSID0123"));
    }

    #[test]
    fn version_report_lists_boot_hashes_and_expansions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let install = dir.path();
        let boot = install.join("boot");
        fs::create_dir_all(&boot).expect("mkdir boot");
        fs::write(boot.join("ffxivboot.ver"), "2026.08.01.0000.0001").expect("boot ver");

        // 缺 boot 檔＝安裝不完整。
        assert!(matches!(
            version_report(install, 1),
            Err(AppError::GameFileMissing { .. })
        ));

        for name in BOOT_FILES {
            fs::write(boot.join(name), b"abc").expect("boot file");
        }
        for (n, ver) in [(1, "2026.09.01.0000.0000"), (2, "2026.09.02.0000.0000")] {
            let ex = install.join(format!("game/sqpack/ex{n}"));
            fs::create_dir_all(&ex).expect("mkdir ex");
            fs::write(ex.join(format!("ex{n}.ver")), ver).expect("ex ver");
        }

        // SHA1("abc") = a9993e364706816aba3e25717850c26c9cd0d89d。
        let h = "3/a9993e364706816aba3e25717850c26c9cd0d89d";
        let expected = format!(
            "2026.08.01.0000.0001=ffxivboot.exe/{h},ffxivboot64.exe/{h},ffxivlauncher64.exe/{h},ffxivupdater64.exe/{h}\n\
             ex1\t2026.09.01.0000.0000\n\
             ex2\t2026.09.02.0000.0000\n"
        );
        assert_eq!(version_report(install, 2).expect("report"), expected);
        assert_eq!(
            version_report(install, 0).expect("report"),
            expected.lines().next().map(|l| format!("{l}\n")).expect("first line")
        );
        // ex3 缺檔：不以基礎版本兜底，拒絕註冊。
        assert!(matches!(
            version_report(install, 3),
            Err(AppError::VersionFileMissing {
                region: Region::Global,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn missing_game_version_is_rejected_before_any_request() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lease = Lease {
            frontier_url: "https://frontier.invalid/{0}/{1}".into(),
            flags: 0,
        };
        // 先檢查 ffxivgame.ver 才建 client 送 OAuth；送了請求會是網路或登入錯誤，不是這個。
        let err = login(&lease, dir.path(), "user", "password", None)
            .await
            .expect_err("err");
        assert!(
            matches!(err, AppError::VersionFileMissing { region: Region::Global, .. }),
            "{err}"
        );
    }

    const LIST_CONTENT_TYPE: &str = "multipart/mixed; boundary=477D80B1_38BC_41d4_8B48_5273ADB89CAC";
    const SECRET_UID: &str = "UIDSECRET0123456789";

    #[test]
    fn session_2xx_with_uid_and_body_hands_back_patch_list_without_uid() {
        let body = b"--477D80B1_38BC_41d4_8B48_5273ADB89CAC\r\n\r\npatch line\r\n".to_vec();
        let result = classify_session(
            200,
            Some(SECRET_UID.into()),
            LIST_CONTENT_TYPE.into(),
            body.clone(),
        )
        .expect("list");
        let VersionCheck::PatchList(list) = result else {
            panic!("應交回清單");
        };
        assert_eq!(list.content_type, LIST_CONTENT_TYPE);
        assert_eq!(list.body, body);
        assert!(!format!("{list:?}").contains(SECRET_UID));
        assert!(!String::from_utf8_lossy(&list.body).contains(SECRET_UID));
    }

    #[test]
    fn session_2xx_with_empty_body_returns_uid() {
        let result = classify_session(200, Some(SECRET_UID.into()), String::new(), Vec::new());
        assert!(matches!(result, Ok(VersionCheck::UpToDate(uid)) if uid == SECRET_UID));
    }

    #[test]
    fn session_2xx_without_uid_is_protocol_error_even_with_list() {
        for uid in [None, Some(String::new())] {
            let result = classify_session(200, uid, LIST_CONTENT_TYPE.into(), b"body".to_vec());
            assert!(matches!(result, Err(AppError::Protocol { .. })));
        }
    }

    #[test]
    fn session_error_statuses_keep_existing_errors() {
        let classify = |status| classify_session(status, None, String::new(), Vec::new());
        assert!(matches!(classify(409), Err(AppError::BootModified)));
        assert!(matches!(
            classify(410),
            Err(AppError::NeedsUpdate {
                region: Region::Global
            })
        ));
        assert!(matches!(
            classify(500),
            Err(AppError::VersionCheckFailed {
                region: Region::Global,
                status: 500
            })
        ));
    }
}
