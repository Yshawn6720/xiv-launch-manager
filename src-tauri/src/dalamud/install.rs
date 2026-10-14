//! 共用安裝原語：下載、暫存目錄、解壓、hash 核對、提交（主 note「程序與狀態」）。
//! 下載與暫存目錄也供程式更新（`crate::update`）使用。
//!
//! 不變量：
//! - 正式目錄（`dalamud/…`、`runtime/…`、`assets/…`）只由 [`Staging::commit`] 的 rename 產生；
//!   目錄存在＝曾經完整核對並提交過。
//! - 呼叫端（衛月安裝、啟動、程式更新）經 app-wide mutex 串行，所以安裝開始前 `staging/` 底下的東西
//!   都是先前中斷的殘留。
//! - 取消（[`CancelToken`]）只在網路等待與步驟之間生效，不會丟棄執行中的 blocking 工作；
//!   最後一個檢查點在提交前，所以取消不會留下半個正式目錄。
//! - 壓縮檔 entry、hash 表的 key 一律經 [`safe_relative_path`]，不可能寫到目的目錄以外。

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{ACCEPT_ENCODING, ACCEPT_RANGES, CONNECTION, CONTENT_RANGE, HeaderMap, RANGE};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha1::Digest;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::error::{AppError, AppResult};
use crate::store::AppPaths;

/// 對 kamori、GitHub、NuGet 一律用本專案自己的 User-Agent，不冒用 XL 身分（protocols §1.4）。
pub const USER_AGENT: &str = "xiv-launch-manager";
/// 衛月本體目錄內的 hash 表（`{相對路徑: MD5 大寫 hex}`）。
pub const HASHES_FILE: &str = "hashes.json";
pub const INJECTOR_EXE: &str = "Dalamud.Injector.exe";
pub const DALAMUD_DLL: &str = "Dalamud.dll";

/// 下載進度回報的最小間隔，避免每個 chunk 都觸發前端事件。
const PROGRESS_STEP: u64 = 512 * 1024;
/// 分段並行下載時，彙總各段進度的週期。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
/// 達到此大小且伺服器支援 byte range 才分段並行。HiNet 直連 Fastly（GitHub release-assets／raw，
/// kamori 也轉址到這裡）時，單條 TCP 連線會隨機在開頭約 0.3–1 MB 後崩落到 60–240 KB/s，任何 client
/// 都一樣（2026-10-13 curl 20 次 6 次慢）；超過約 1 MB 的檔案走單連線，就可能大半時間卡在崩落後的速度。
/// 分段後每段只剩 1/8，崩落的段再由換線（[`STALL_WINDOW`]）接手。更小的檔案崩落後最多剩約 0.7 MB
/// （以 60 KB/s 計約 12 秒），不值得讓每個小 asset 都多付一個請求。
const PARALLEL_MIN_SIZE: u64 = 1024 * 1024;
/// 並行段數。各段各自一條連線：同時段新開的連線多半是快的（4–9 MB/s），崩落只拖慢自己那段。
const PARALLEL_SEGMENTS: u64 = 8;
/// 換線偵測的時間窗：分段連線每滿一個時間窗量一次速度，太慢就斷線，以新連線 Range 續抓該段剩餘範圍。
/// 崩落後整條連線持續停在 60–240 KB/s，1 秒足以分辨；更短容易把正常連線偶發的重傳停頓當成崩落。
/// `read_timeout` 只抓得到完全沒資料，抓不到「還在進但很慢」。
const STALL_WINDOW: Duration = Duration::from_secs(1);
/// 換線門檻上限（bytes/s）：落在實測快（4–9 MB/s）與慢（60–240 KB/s）之間，兩側各約 4 倍餘裕。
const STALL_RATE_CAP: u64 = 1024 * 1024;
/// 門檻也不超過本次下載單條連線最高速度的 1/8：線路本身慢、各段平分頻寬而都低於上限時，正常連線不會
/// 被當成崩落；崩落是 17 倍以上的落差（4 MB/s → 240 KB/s），1/8 仍抓得到。
const STALL_PEAK_DIVISOR: u64 = 8;
/// 時間窗內收到至少這麼多，才拿當下的速度更新最高速度：資料太少時，速度受握手與緩衝影響而不可靠。
/// 比崩落前的快速段（至少 0.3 MB）小，崩落的連線在崩落前也量得到。
const PEAK_SAMPLE_BYTES: u64 = 256 * 1024;
/// 一條連線被換掉前至少收到這麼多才算有進展：約一個 TCP 初始壅塞窗（10 個封包 ≈ 14.6 KB），
/// 一秒內連這都收不滿的連線等於沒在動。
const STALL_MIN_PROGRESS: u64 = 16 * 1024;
/// 同一段連續換線都沒有進展的上限，超過回錯誤：連續幾條新連線都幾乎沒資料，代表網路本身斷了，換線沒用。
/// 有進展的換線不計入：每次至少前進 [`STALL_MIN_PROGRESS`]，總次數仍受段長限制。2026-10-13 smoke 中
/// 約一半的換線發生在新連線第一秒只收到 160–260 KB 時；那些連線慢但有在動，計入次數會讓下得完的下載失敗。
const STALL_MAX_RETRIES: u32 = 3;

/// 下載取消的世代計數（app 全域一份，不需要 app-wide mutex）。[`Self::cancel`] 遞增世代；
/// 每次下載流程開始時以 [`Self::token`] 記下當下世代，之後世代改變就是被取消。
/// 沒有下載進行時取消只會遞增世代，下一次流程從新世代開始，不受影響。
pub struct DownloadCancel(watch::Sender<u64>);

impl Default for DownloadCancel {
    fn default() -> Self {
        Self(watch::Sender::new(0))
    }
}

impl DownloadCancel {
    pub fn token(&self) -> CancelToken {
        let generation = self.0.subscribe();
        let start = *generation.borrow();
        CancelToken { generation, start }
    }

    pub fn cancel(&self) {
        self.0.send_modify(|g| *g = g.wrapping_add(1));
    }
}

/// 一次下載流程的取消訊號。網路等待用 [`Self::race`] 立即中斷；blocking 的解壓／核對不能半途丟棄
/// （背景執行緒仍在寫暫存），改在步驟之間呼叫 [`Self::check`]。
#[derive(Clone)]
pub struct CancelToken {
    generation: watch::Receiver<u64>,
    start: u64,
}

impl CancelToken {
    pub fn check(&self) -> AppResult<()> {
        if *self.generation.borrow() == self.start {
            Ok(())
        } else {
            Err(AppError::Cancelled)
        }
    }

    async fn cancelled(&self) {
        let mut generation = self.generation.clone();
        let closed = generation.wait_for(|g| *g != self.start).await.is_err();
        // 發送端隨 app 狀態存活；被 drop 代表不會再有取消，永遠等下去。
        if closed {
            std::future::pending::<()>().await;
        }
    }

    /// 與取消競速：取消時直接丟棄 `fut`，所以只能包純網路的 future。
    pub async fn race<T>(&self, fut: impl Future<Output = AppResult<T>>) -> AppResult<T> {
        tokio::select! {
            biased;
            () = self.cancelled() => Err(AppError::Cancelled),
            result = fut => result,
        }
    }
}

/// 安裝進度。`total` 未知（伺服器沒給長度）時為 `None`。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub stage: String,
    pub done: u64,
    pub total: Option<u64>,
}

pub type ProgressFn = dyn Fn(Progress) + Send + Sync;

pub fn report(progress: &ProgressFn, stage: &str, done: u64, total: Option<u64>) {
    progress(Progress {
        stage: stage.to_string(),
        done,
        total,
    });
}

/// 只設連線與讀取逾時，不設總逾時：runtime 套件數十 MB，慢速網路下總時間無上限是預期行為。
/// 只用 HTTP/1.1：分段並行與換線都要求各段各自一條 TCP 連線；HTTP/2 會把各段多工到同一條連線，
/// 那條連線一崩落（見 [`PARALLEL_MIN_SIZE`]）就全部一起慢，換線也換不掉。
/// （2026-10-13 實測 GitHub raw 58 MB 兩輪：HTTP/2 803、86 KB/s，後者與同時段單連線 81 KB/s 相當；
/// HTTP/1.1 490、306 KB/s。）
pub fn http_client() -> AppResult<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .http1_only()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| AppError::network("建立 HTTP client", e))
}

/// 取小型回應全文（版本資訊、hash 表）。錯誤只帶分類，不帶 URL 與回應原文。
pub async fn fetch_bytes(request: reqwest::RequestBuilder, context: &str) -> AppResult<Vec<u8>> {
    let response = request
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| AppError::network(context, e))?;
    let body = response
        .bytes()
        .await
        .map_err(|e| AppError::network(context, e))?;
    Ok(Vec::from(body))
}

/// JSON 解析失敗只回分類；容許 UTF-8 BOM（上游部分檔案由 .NET 工具產生）。
pub fn parse_json<T: DeserializeOwned>(bytes: &[u8], context: &str) -> AppResult<T> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    serde_json::from_slice(bytes).map_err(|_| AppError::protocol(context))
}

pub async fn fetch_json<T: DeserializeOwned>(
    request: reqwest::RequestBuilder,
    context: &str,
) -> AppResult<T> {
    parse_json(&fetch_bytes(request, context).await?, context)
}

/// 下載到 `dest`，回傳位元組數。夠大且伺服器宣告 `Accept-Ranges: bytes` 時分段並行：各段對轉址後的
/// 最終 URL 發 Range（不必每段重走 kamori），某段連線太慢就換新連線續抓該段剩餘範圍，任一段回應不符
/// 就整檔改單連線重下。各段要各自一條連線才有效果，所以 `client` 應為 [`http_client`]（HTTP/1.1）。
/// 完整性仍靠呼叫端核對（衛月用 hash、程式更新用簽章）。
/// 取消時回 `Cancelled`；半成品留在 `dest`，由呼叫端的 [`Staging`] 清掉。
/// 寫檔用 std::fs：tokio 未啟用 io-util，且 chunk 寫入很短。
pub async fn download_to_file(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    stage: &str,
    progress: &ProgressFn,
    cancel: &CancelToken,
) -> AppResult<u64> {
    download(client, url, dest, None, stage, progress, cancel).await
}

/// 同 [`download_to_file`]，但長度事先已知（如 patch 清單長度）：每個決定配置或開始寫入的整檔回應
/// （首次回應＝分段時 `set_len` 的長度來源、分段不符退回單連線的重新 GET），`Content-Length` 都須等於
/// `length`，不符即在建立、配置或寫入 `dest` 前回錯。
pub async fn download_to_file_exact(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    length: u64,
    stage: &str,
    progress: &ProgressFn,
    cancel: &CancelToken,
) -> AppResult<u64> {
    download(client, url, dest, Some(length), stage, progress, cancel).await
}

async fn download(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    expected: Option<u64>,
    stage: &str,
    progress: &ProgressFn,
    cancel: &CancelToken,
) -> AppResult<u64> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io("建立下載目錄", e))?;
    }
    let response = send(client.get(url), stage, cancel).await?;
    check_length(&response, expected, stage)?;
    let Some(total) = parallel_size(&response) else {
        return stream_to_file(response, dest, stage, progress, cancel).await;
    };
    let final_url = response.url().clone();
    // 只需要標頭；丟棄回應會關掉這條連線，各段另開連線。
    drop(response);
    if download_ranges(client, &final_url, dest, total, stage, progress, cancel).await? {
        return Ok(total);
    }
    let response = send(client.get(url), stage, cancel).await?;
    check_length(&response, expected, stage)?;
    stream_to_file(response, dest, stage, progress, cancel).await
}

/// 長度事先已知時，整檔回應的 `Content-Length` 須正好是該長度（缺 `Content-Length` 也算不符）。
fn check_length(response: &reqwest::Response, expected: Option<u64>, stage: &str) -> AppResult<()> {
    match expected {
        Some(length) if response.content_length() != Some(length) => {
            Err(AppError::protocol(format!("{stage}：回應長度與預期不符")))
        }
        _ => Ok(()),
    }
}

async fn send(
    request: reqwest::RequestBuilder,
    stage: &str,
    cancel: &CancelToken,
) -> AppResult<reqwest::Response> {
    cancel
        .race(async {
            request
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .map_err(|e| AppError::network(stage, e))
        })
        .await
}

/// 值得分段時回傳總長度：200、長度已知（未經解壓）、夠大、宣告支援 byte range。
fn parallel_size(response: &reqwest::Response) -> Option<u64> {
    let ranges = response
        .headers()
        .get(ACCEPT_RANGES)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|unit| unit.trim().eq_ignore_ascii_case("bytes")));
    response
        .content_length()
        .filter(|&n| ranges && n >= PARALLEL_MIN_SIZE && response.status() == StatusCode::OK)
}

/// 單連線串流寫入 `dest`（覆寫）。
async fn stream_to_file(
    mut response: reqwest::Response,
    dest: &Path,
    stage: &str,
    progress: &ProgressFn,
    cancel: &CancelToken,
) -> AppResult<u64> {
    let total = response.content_length();
    let mut file = BufWriter::new(File::create(dest).map_err(|e| AppError::io("建立下載檔", e))?);
    let mut done = 0u64;
    let mut reported = 0u64;
    report(progress, stage, 0, total);
    while let Some(chunk) = cancel
        .race(async { response.chunk().await.map_err(|e| AppError::network(stage, e)) })
        .await?
    {
        file.write_all(&chunk)
            .map_err(|e| AppError::io("寫入下載檔", e))?;
        done += chunk.len() as u64;
        if done - reported >= PROGRESS_STEP {
            reported = done;
            report(progress, stage, done, total);
        }
    }
    file.flush().map_err(|e| AppError::io("寫入下載檔", e))?;
    report(progress, stage, done, total);
    Ok(done)
}

/// 分段下載各段共用的狀態。
struct RangeJob {
    client: reqwest::Client,
    url: reqwest::Url,
    dest: PathBuf,
    total: u64,
    stage: String,
    /// 各段已寫入的位元組總和（進度）。
    done: AtomicU64,
    /// 本次下載任一連線量到的最高速度（bytes/s），換線門檻的參考（[`STALL_PEAK_DIVISOR`]）。
    peak_rate: AtomicU64,
}

impl RangeJob {
    /// 一個時間窗的平均速度低於此值（bytes/s）就換線。至少 1：整個時間窗沒收到資料一律換線。
    fn stall_rate(&self) -> u64 {
        (self.peak_rate.load(Ordering::Relaxed) / STALL_PEAK_DIVISOR).clamp(1, STALL_RATE_CAP)
    }
}

/// 一條連線的結果。
enum Attempt {
    /// 收完要求的範圍。
    Complete,
    /// 回應不是要求的範圍：狀態、長度、`Content-Range` 不符，或資料多於／少於要求。
    Mismatch,
    /// 太慢而斷線；`received`＝這條連線收到的位元組數。
    Stalled { received: u64 },
}

/// 分段並行下載到預先配置成 `total` 大小的 `dest`。回 `false`＝某段回應不符，呼叫端改單連線重下。
/// 不論結果，回傳前所有分段工作都已結束（檔案 handle 已關閉，暫存才刪得掉）。
async fn download_ranges(
    client: &reqwest::Client,
    url: &reqwest::Url,
    dest: &Path,
    total: u64,
    stage: &str,
    progress: &ProgressFn,
    cancel: &CancelToken,
) -> AppResult<bool> {
    File::create(dest)
        .and_then(|file| file.set_len(total))
        .map_err(|e| AppError::io("建立下載檔", e))?;
    let job = Arc::new(RangeJob {
        client: client.clone(),
        url: url.clone(),
        dest: dest.to_path_buf(),
        total,
        stage: stage.to_string(),
        done: AtomicU64::new(0),
        peak_rate: AtomicU64::new(0),
    });
    let segment = total.div_ceil(PARALLEL_SEGMENTS);
    let mut tasks = JoinSet::new();
    let mut start = 0;
    while start < total {
        let end = start.saturating_add(segment).min(total) - 1;
        tasks.spawn(fetch_range(Arc::clone(&job), start, end));
        start = end + 1;
    }

    report(progress, stage, 0, Some(total));
    let mut ticker = tokio::time::interval(PROGRESS_INTERVAL);
    let mut reported = 0u64;
    let result = loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => break Err(AppError::Cancelled),
            joined = tasks.join_next() => match joined {
                None => break Ok(true),
                Some(Ok(Ok(true))) => {}
                Some(Ok(Ok(false))) => break Ok(false),
                Some(Ok(Err(e))) => break Err(e),
                Some(Err(_)) => {
                    break Err(AppError::Install {
                        context: format!("{stage}：分段下載中斷"),
                    });
                }
            },
            _ = ticker.tick() => {
                let now = job.done.load(Ordering::Relaxed);
                if now - reported >= PROGRESS_STEP {
                    reported = now;
                    report(progress, stage, now, Some(total));
                }
            }
        }
    };
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    if matches!(result, Ok(true)) {
        report(progress, stage, total, Some(total));
    }
    result
}

/// 下載 `[start, end]`（含端點）寫到 `dest` 的同一 offset。連線太慢（[`STALL_WINDOW`]）就斷線，以新連線
/// Range 續抓剩餘範圍，已寫入的不重下；連續超過 [`STALL_MAX_RETRIES`] 次換線都沒有進展回錯誤。
/// 任一條連線的回應不符回 `false`。
async fn fetch_range(job: Arc<RangeJob>, start: u64, end: u64) -> AppResult<bool> {
    let mut file = OpenOptions::new()
        .write(true)
        .open(&job.dest)
        .map_err(|e| AppError::io("開啟下載檔", e))?;
    let mut next = start;
    let mut idle_retries = 0;
    loop {
        match fetch_from(&job, &mut file, &mut next, end).await? {
            Attempt::Complete => return Ok(true),
            Attempt::Mismatch => return Ok(false),
            Attempt::Stalled { received } => {
                idle_retries = if received >= STALL_MIN_PROGRESS { 0 } else { idle_retries + 1 };
                if idle_retries > STALL_MAX_RETRIES {
                    return Err(AppError::Network {
                        context: job.stage.clone(),
                        message: format!("連線速度持續過慢，重新連線 {STALL_MAX_RETRIES} 次仍未改善"),
                    });
                }
            }
        }
    }
}

/// 以一條連線抓 `[*next, end]`，邊收邊寫並推進 `*next`。
async fn fetch_from(
    job: &RangeJob,
    file: &mut File,
    next: &mut u64,
    end: u64,
) -> AppResult<Attempt> {
    let start = *next;
    let mut response = job
        .client
        .get(job.url.clone())
        .header(RANGE, format!("bytes={start}-{end}"))
        // Range 針對原始位元組；壓縮傳輸會讓長度對不上。
        .header(ACCEPT_ENCODING, "identity")
        // 換線要的是新的 TCP 連線：分段連線用完就關，不回 pool，續抓才不會拿到已下載過一段的舊連線。
        .header(CONNECTION, "close")
        .send()
        .await
        .map_err(|e| AppError::network(job.stage.as_str(), e))?;
    if response.status() != StatusCode::PARTIAL_CONTENT
        || response.content_length() != Some(end - start + 1)
        || !content_range_is(response.headers(), start, end, job.total)
    {
        return Ok(Attempt::Mismatch);
    }
    file.seek(SeekFrom::Start(start))
        .map_err(|e| AppError::io("寫入下載檔", e))?;
    let mut out = BufWriter::new(&mut *file);
    let mut window_start = Instant::now();
    let mut window_bytes = 0u64;
    let attempt = loop {
        let deadline = window_start + STALL_WINDOW;
        // 時間窗內沒有新資料也要量速度。chunk() 可安全取消：還沒收到的資料留在連線上。
        if let Ok(chunk) = tokio::time::timeout_at(deadline, response.chunk()).await {
            let Some(chunk) = chunk.map_err(|e| AppError::network(job.stage.as_str(), e))? else {
                break if *next > end { Attempt::Complete } else { Attempt::Mismatch };
            };
            let len = chunk.len() as u64;
            if len > end + 1 - *next {
                break Attempt::Mismatch;
            }
            out.write_all(&chunk)
                .map_err(|e| AppError::io("寫入下載檔", e))?;
            *next += len;
            window_bytes += len;
            job.done.fetch_add(len, Ordering::Relaxed);
        }
        let now = Instant::now();
        let rate = bytes_per_sec(window_bytes, now - window_start);
        // 時間窗內收到夠多就先取樣，崩落前那 0.3–1 MB 的快速段與很快下完的段才量得到。
        if window_bytes >= PEAK_SAMPLE_BYTES || now >= deadline {
            job.peak_rate.fetch_max(rate, Ordering::Relaxed);
        }
        if now >= deadline {
            if rate < job.stall_rate() {
                break Attempt::Stalled { received: *next - start };
            }
            window_start = now;
            window_bytes = 0;
        }
    };
    out.flush().map_err(|e| AppError::io("寫入下載檔", e))?;
    Ok(attempt)
}

fn bytes_per_sec(bytes: u64, elapsed: Duration) -> u64 {
    let micros = elapsed.as_micros().max(1);
    u64::try_from(u128::from(bytes) * 1_000_000 / micros).unwrap_or(u64::MAX)
}

/// `Content-Range: bytes start-end/total` 是否正好是要求的範圍（總長也要相同，否則檔案已變動）。
fn content_range_is(headers: &HeaderMap, start: u64, end: u64, total: u64) -> bool {
    let Some((range, size)) = headers
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().strip_prefix("bytes "))
        .and_then(|v| v.split_once('/'))
    else {
        return false;
    };
    let parse = |s: &str| s.trim().parse::<u64>().ok();
    range
        .split_once('-')
        .is_some_and(|(a, b)| parse(a) == Some(start) && parse(b) == Some(end))
        && parse(size) == Some(total)
}

/// CPU／磁碟密集的解壓與核對放到 blocking 執行緒，避免卡住 async runtime。
pub async fn run_blocking<T, F>(context: &str, f: F) -> AppResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> AppResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| AppError::Install {
            context: context.to_string(),
        })?
}

/// 版本字串、tag、profile id 等要當成單一路徑元件使用的值：只允許 `[A-Za-z0-9._-]`，
/// 且不得只由 `.` 組成（擋 `.`、`..`）。
pub fn is_safe_component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.chars().all(|c| c == '.')
        && s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// 壓縮檔 entry 或 hash 表 key 轉成相對路徑：`\` 與 `/` 視為相同；含 `..`、絕對路徑、
/// 磁碟代號、`:`（含 NTFS 資料流）一律拒絕。只由 `.`／空白組成的元件也拒絕，
/// 因為 Win32 會去掉元件尾端的 `.` 與空白（`.. ` 會變成 `..`）。
pub fn safe_relative_path(name: &str) -> Option<PathBuf> {
    let normalized = name.replace('\\', "/");
    if normalized.starts_with('/') {
        return None;
    }
    let mut out = PathBuf::new();
    for segment in normalized.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment.trim_end_matches(['.', ' ']).is_empty()
            || segment.contains(':')
            || segment.contains('\0')
        {
            return None;
        }
        out.push(segment);
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

fn unsafe_entry() -> AppError {
    AppError::Install {
        context: "壓縮檔含不安全的路徑".into(),
    }
}

/// 一次安裝用的唯一暫存目錄（`staging/<label>-<uuid>`）。drop 時整個刪除：
/// 提交成功後只剩下載的壓縮檔，失敗或 future 被取消時連同半成品一起清掉。
pub struct Staging {
    dir: PathBuf,
}

impl Staging {
    pub fn create(paths: &AppPaths, label: &str) -> AppResult<Self> {
        let dir = paths
            .staging_dir()
            .join(format!("{label}-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&dir).map_err(|e| AppError::io("建立暫存目錄", e))?;
        Ok(Self { dir })
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// 提交點：把暫存目錄內的 `staged` rename 成 `target`。`target` 已存在時（核對失敗的舊版）
    /// 先移開；移開失敗代表仍被使用，整個提交失敗，舊目錄保持原樣。
    pub fn commit(self, paths: &AppPaths, staged: &Path, target: &Path) -> AppResult<()> {
        debug_assert!(staged.starts_with(&self.dir));
        let parent = target.parent().ok_or_else(|| AppError::Install {
            context: "安裝目錄沒有上層目錄".into(),
        })?;
        fs::create_dir_all(parent).map_err(|e| AppError::io("建立安裝目錄", e))?;
        if target.exists() {
            let aside = move_aside(paths, target)?;
            remove_best_effort(&aside);
        }
        fs::rename(staged, target).map_err(|e| AppError::io("提交安裝目錄", e))
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        // 刪除失敗只會留下殘留；暫存名稱唯一不影響之後安裝，下次 purge_stale 會再清。
        // quality-allow: Drop 無法回傳錯誤，清理失敗由下次 purge_stale 補救
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// 把正式目錄 rename 到 `staging/retired-<uuid>`。Windows 上目錄內有檔案被開啟（遊戲正在用）時會失敗，
/// 此時正式目錄不受影響。
pub fn move_aside(paths: &AppPaths, target: &Path) -> AppResult<PathBuf> {
    let root = paths.staging_dir();
    fs::create_dir_all(&root).map_err(|e| AppError::io("建立暫存目錄", e))?;
    let aside = root.join(format!("retired-{}", uuid::Uuid::new_v4().simple()));
    fs::rename(target, &aside).map_err(|e| AppError::io("移開舊版本（可能正被遊戲使用）", e))?;
    Ok(aside)
}

fn remove_best_effort(path: &Path) {
    let result = if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    // 已移出正式路徑，刪不掉只是暫存殘留，下次 purge_stale 再清。
    // quality-allow: best-effort 清理，失敗不影響正式目錄與本次安裝結果
    let _ = result;
}

/// 清掉先前中斷留下的暫存。只能在 app-wide mutex 內呼叫（否則可能刪到進行中的安裝）。
pub fn purge_stale(paths: &AppPaths) {
    // staging 不存在＝沒有殘留；讀不到目錄也不影響本次安裝（暫存名稱唯一）。
    // quality-allow: 殘留清理是 best-effort，讀不到目錄時沒有可清的東西
    let Ok(entries) = fs::read_dir(paths.staging_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        remove_best_effort(&entry.path());
    }
}

/// 解壓整個 zip 到 `dest`。
pub fn extract_zip(archive: &Path, dest: &Path) -> AppResult<()> {
    extract_zip_mapped(archive, |rel| Some(dest.join(rel)))
}

/// 解壓 zip，`map` 把已驗證的相對路徑對應到輸出路徑；回傳 `None` 的 entry 略過。
/// 任何 entry 路徑不安全（即使會被略過）整個壓縮檔都視為不可信而拒絕。
/// 讀完 entry 時 zip crate 會核對 CRC32。
pub fn extract_zip_mapped(
    archive: &Path,
    mut map: impl FnMut(&Path) -> Option<PathBuf>,
) -> AppResult<()> {
    let file = File::open(archive).map_err(|e| AppError::io("開啟壓縮檔", e))?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file)).map_err(zip_error)?;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(zip_error)?;
        let name = entry.name().map_err(zip_error)?.into_owned();
        let rel = safe_relative_path(&name).ok_or_else(unsafe_entry)?;
        if entry.is_symlink() {
            return Err(unsafe_entry());
        }
        let Some(out) = map(&rel) else {
            continue;
        };
        if entry.is_dir() {
            fs::create_dir_all(&out).map_err(|e| AppError::io("建立解壓目錄", e))?;
        } else {
            write_entry(&mut entry, &out)?;
        }
    }
    Ok(())
}

fn zip_error(e: zip::result::ZipError) -> AppError {
    AppError::Install {
        context: format!("讀取 zip：{e}"),
    }
}

fn write_entry(reader: &mut dyn Read, out: &Path) -> AppResult<()> {
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io("建立解壓目錄", e))?;
    }
    let mut writer =
        BufWriter::new(File::create(out).map_err(|e| AppError::io("建立解壓檔案", e))?);
    io::copy(reader, &mut writer).map_err(|e| AppError::io("解壓檔案", e))?;
    writer.flush().map_err(|e| AppError::io("解壓檔案", e))
}

/// 解壓整個 7z 到 `dest`（7z 內建 CRC 於讀完 entry 時核對）。
pub fn extract_7z(archive: &Path, dest: &Path) -> AppResult<()> {
    let file = File::open(archive).map_err(|e| AppError::io("開啟壓縮檔", e))?;
    let mut reader =
        sevenz_rust2::ArchiveReader::new(BufReader::new(file), sevenz_rust2::Password::empty())
            .map_err(sevenz_error)?;
    // 回呼只能回 sevenz 的錯誤型別；本模組的錯誤先存起來，再用一個 io 錯誤中止迴圈。
    let mut failure: Option<AppError> = None;
    let result = reader.for_each_entries(|entry, data| match extract_7z_entry(entry, data, dest) {
        Ok(()) => Ok(true),
        Err(e) => {
            failure = Some(e);
            Err(io::Error::other("中止解壓").into())
        }
    });
    if let Some(e) = failure {
        return Err(e);
    }
    result.map_err(sevenz_error)
}

fn extract_7z_entry(
    entry: &sevenz_rust2::ArchiveEntry,
    data: &mut dyn Read,
    dest: &Path,
) -> AppResult<()> {
    // anti-item 是增量封存的「刪除」標記，全新解壓時沒有對應檔案。
    if entry.is_anti_item() {
        return Ok(());
    }
    let rel = safe_relative_path(entry.name()).ok_or_else(unsafe_entry)?;
    let out = dest.join(rel);
    if entry.is_directory() {
        fs::create_dir_all(&out).map_err(|e| AppError::io("建立解壓目錄", e))
    } else {
        write_entry(data, &out)
    }
}

fn sevenz_error(e: sevenz_rust2::Error) -> AppError {
    AppError::Install {
        context: format!("讀取 7z：{e}"),
    }
}

#[derive(Debug, Clone, Copy)]
pub enum HashAlgo {
    Md5,
    Sha1,
}

/// 檔案 hash，大寫 hex。
pub fn file_hash(path: &Path, algo: HashAlgo) -> io::Result<String> {
    match algo {
        HashAlgo::Md5 => digest_file::<md5::Md5>(path),
        HashAlgo::Sha1 => digest_file::<sha1::Sha1>(path),
    }
}

fn digest_file<D: Digest>(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = D::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode_upper(hasher.finalize()))
}

/// 逐檔核對。`expected` 為 `None`（上游沒給 hash）時只要求檔案存在。
/// 缺檔或不符回 `Integrity { file }`，`file` 是表上的相對路徑。
pub fn verify_files<'a>(
    root: &Path,
    entries: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
    algo: HashAlgo,
) -> AppResult<()> {
    for (name, expected) in entries {
        let rel = safe_relative_path(name).ok_or_else(unsafe_entry)?;
        let path = root.join(rel);
        let integrity = || AppError::Integrity {
            file: name.to_string(),
        };
        let Some(expected) = expected else {
            if path.is_file() {
                continue;
            }
            return Err(integrity());
        };
        match file_hash(&path, algo) {
            Ok(actual) if actual.eq_ignore_ascii_case(expected) => {}
            Ok(_) => return Err(integrity()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(integrity()),
            Err(e) => return Err(AppError::io(format!("讀取 {name}"), e)),
        }
    }
    Ok(())
}

/// `{相對路徑: MD5 大寫 hex}`（goatcorp／yanmucorp 本體與 goatcorp runtime 共用格式）。
pub type HashTable = BTreeMap<String, String>;

/// 空表會讓核對無條件通過，視為格式錯誤。
pub fn parse_hash_table(bytes: &[u8], context: &str) -> AppResult<HashTable> {
    let table: HashTable = parse_json(bytes, context)?;
    if table.is_empty() {
        return Err(AppError::protocol(context));
    }
    Ok(table)
}

pub fn verify_hash_table(root: &Path, table: &HashTable) -> AppResult<()> {
    verify_files(
        root,
        table.iter().map(|(k, v)| (k.as_str(), Some(v.as_str()))),
        HashAlgo::Md5,
    )
}

/// hash 表是否列出某個相對路徑（key 可能用反斜線）。
pub fn table_lists(table: &HashTable, file: &str) -> bool {
    table.keys().any(|k| k.replace('\\', "/") == file)
}

/// 以目錄內 `hashes.json` 核對衛月本體；表上必須列出 Injector，否則空殼也會通過。
pub fn verify_dalamud_body(dir: &Path) -> AppResult<()> {
    let bytes = fs::read(dir.join(HASHES_FILE)).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            AppError::Integrity {
                file: HASHES_FILE.into(),
            }
        } else {
            AppError::io("讀取 hashes.json", e)
        }
    })?;
    let table = parse_hash_table(&bytes, "Dalamud hashes.json")
        .map_err(|_| AppError::Integrity {
            file: HASHES_FILE.into(),
        })?;
    if !table_lists(&table, INJECTOR_EXE) {
        return Err(AppError::Integrity {
            file: INJECTOR_EXE.into(),
        });
    }
    verify_hash_table(dir, &table)
}

/// 已提交目錄的重新核對結果：完整性不符回 `false`（呼叫端重裝），其他錯誤（I/O）往上拋。
pub fn still_complete(result: AppResult<()>) -> AppResult<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(AppError::Integrity { .. }) => Ok(false),
        Err(e) => Err(e),
    }
}

/// 已提交的衛月本體每次以內含 hashes.json 重核；不存在或不符（被改動、刪檔）回 `false`，由呼叫端重裝。
pub async fn committed_body_ok(target: &Path) -> AppResult<bool> {
    if !target.is_dir() {
        return Ok(false);
    }
    let dir = target.to_path_buf();
    run_blocking("核對 Dalamud 本體", move || still_complete(verify_dalamud_body(&dir))).await
}

/// 衛月本體的提交流程（兩來源共用）：核對 `content` 後 rename 成 `target`。
/// 核對失敗或已取消時 `staging` 被 drop，暫存整個刪除，`target` 不會出現。
pub fn commit_dalamud_body(
    paths: &AppPaths,
    staging: Staging,
    content: &Path,
    target: &Path,
    cancel: &CancelToken,
) -> AppResult<()> {
    verify_dalamud_body(content)?;
    cancel.check()?;
    staging.commit(paths, content, target)
}

#[cfg(test)]
mod tests {
    use std::io::BufRead;
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;
    use std::time::Instant;

    use super::*;

    fn md5_hex(data: &[u8]) -> String {
        hex::encode_upper(md5::Md5::digest(data))
    }

    fn write(path: &Path, data: &[u8]) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, data).expect("write");
    }

    fn sample_tree(root: &Path) -> HashTable {
        write(&root.join("Dalamud.Injector.exe"), b"injector");
        write(&root.join("runtimes/win-x64/native/a.dll"), b"native");
        let mut table = HashTable::new();
        table.insert("Dalamud.Injector.exe".into(), md5_hex(b"injector"));
        // 上游 hash 表用反斜線。
        table.insert(
            "runtimes\\win-x64\\native\\a.dll".into(),
            md5_hex(b"native"),
        );
        table
    }

    #[test]
    fn hash_table_matches_including_backslash_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let table = sample_tree(dir.path());
        verify_hash_table(dir.path(), &table).expect("verify");
    }

    #[test]
    fn hash_mismatch_reports_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let table = sample_tree(dir.path());
        write(&dir.path().join("runtimes/win-x64/native/a.dll"), b"tampered");
        match verify_hash_table(dir.path(), &table) {
            Err(AppError::Integrity { file }) => {
                assert_eq!(file, "runtimes\\win-x64\\native\\a.dll")
            }
            other => panic!("expected integrity error, got {other:?}"),
        }
    }

    #[test]
    fn missing_file_is_an_integrity_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut table = sample_tree(dir.path());
        table.insert("missing.dll".into(), md5_hex(b""));
        assert!(matches!(
            verify_hash_table(dir.path(), &table),
            Err(AppError::Integrity { file }) if file == "missing.dll"
        ));
    }

    fn build_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(path).expect("create zip");
        let mut zip = zip::ZipWriter::new(file);
        for (name, data) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .expect("start");
            zip.write_all(data).expect("write");
        }
        zip.finish().expect("finish");
    }

    #[test]
    fn zip_path_traversal_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (i, bad) in ["../evil.txt", "sub/../../evil.txt", "/evil.txt", "C:/evil.txt"]
            .iter()
            .enumerate()
        {
            let archive = dir.path().join(format!("bad{i}.zip"));
            build_zip(&archive, &[("ok.txt", b"ok"), (bad, b"evil")]);
            let dest = dir.path().join(format!("out{i}"));
            assert!(
                extract_zip(&archive, &dest).is_err(),
                "{bad:?} should be rejected"
            );
            assert!(!dir.path().join("evil.txt").exists());
        }
    }

    #[test]
    fn failed_verification_leaves_no_target_and_no_staging() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::new(dir.path().to_path_buf());
        let staging = Staging::create(&paths, "test").expect("staging");
        let staging_dir = staging.path().to_path_buf();
        let content = staging_dir.join("content");
        let table = sample_tree(&content);
        write(&content.join("Dalamud.Injector.exe"), b"corrupted");
        write(
            &content.join(HASHES_FILE),
            &serde_json::to_vec(&table).expect("json"),
        );
        let target = paths.dalamud_dir("goatcorp", "1.0.0");

        let cancel = DownloadCancel::default();
        let result = commit_dalamud_body(&paths, staging, &content, &target, &cancel.token());

        assert!(matches!(result, Err(AppError::Integrity { .. })));
        assert!(!target.exists());
        assert!(!staging_dir.exists());
    }

    #[test]
    fn cancel_only_affects_tokens_taken_before_it() {
        let cancel = DownloadCancel::default();
        // 沒有下載進行時取消是 no-op。
        cancel.cancel();
        let token = cancel.token();
        assert!(token.check().is_ok());
        cancel.cancel();
        assert!(matches!(token.check(), Err(AppError::Cancelled)));
        assert!(cancel.token().check().is_ok());
    }

    #[test]
    fn cancelled_message_is_what_the_frontend_matches() {
        assert_eq!(AppError::Cancelled.to_string(), "已取消");
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum RangeMode {
        /// 宣告並正確支援 Range。
        Supported,
        /// 不宣告 Accept-Ranges，一律 200 全檔。
        Unsupported,
        /// 宣告 Accept-Ranges，但 Range 請求仍回 200 全檔。
        Ignored,
        /// 同 `Ignored`，但第二個起的整檔請求回較長的檔（分段作廢、退回單連線時檔案已變動）。
        IgnoredThenLonger,
    }

    /// 本機 HTTP/1.1 測試伺服器（std 執行緒；tokio 未啟用 net）。`ranges` 依序記錄每個請求的 Range 標頭。
    struct Stub {
        url: String,
        ranges: Arc<Mutex<Vec<Option<String>>>>,
    }

    impl Stub {
        fn ranges(&self) -> Vec<Option<String>> {
            self.ranges.lock().expect("lock").clone()
        }
    }

    /// 每送出 16 KiB 停 `pause`，模擬每條連線限速。
    fn serve(body: Arc<Vec<u8>>, mode: RangeMode, pause: Duration) -> Stub {
        spawn_stub(body, mode, pause, None)
    }

    /// 依 Range 起點決定哪些連線會崩落：回傳 `Some(n)`＝送出 n bytes 後變極慢（[`DRIP`]）。
    type Collapse = Arc<dyn Fn(usize) -> Option<usize> + Send + Sync>;

    /// 崩落後每 100 ms 只送 1 KiB（約 10 KB/s），比實測最慢的 60 KB/s 還慢。
    const DRIP: (usize, Duration) = (1024, Duration::from_millis(100));

    /// 正確支援 Range，但 `collapse` 指定的連線送出一段後變極慢（模擬 HiNet→Fastly 的連線崩落）。
    fn serve_collapsing(body: Arc<Vec<u8>>, collapse: Collapse) -> Stub {
        spawn_stub(body, RangeMode::Supported, Duration::ZERO, Some(collapse))
    }

    fn spawn_stub(
        body: Arc<Vec<u8>>,
        mode: RangeMode,
        pause: Duration,
        collapse: Option<Collapse>,
    ) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/latest.zip", listener.local_addr().expect("addr"));
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&ranges);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (body, log, collapse) = (Arc::clone(&body), Arc::clone(&log), collapse.clone());
                std::thread::spawn(move || {
                    respond(stream, &body, mode, pause, collapse.as_ref(), &log);
                });
            }
        });
        Stub { url, ranges }
    }

    fn respond(
        mut stream: TcpStream,
        body: &[u8],
        mode: RangeMode,
        pause: Duration,
        collapse: Option<&Collapse>,
        log: &Mutex<Vec<Option<String>>>,
    ) {
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut range = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("range")
            {
                range = Some(value.trim().to_string());
            }
        }
        let earlier_plain = {
            let mut log = log.lock().expect("lock");
            let n = log.iter().filter(|r| r.is_none()).count();
            log.push(range.clone());
            n
        };
        let accept = match mode {
            RangeMode::Unsupported => "",
            RangeMode::Supported | RangeMode::Ignored | RangeMode::IgnoredThenLonger => {
                "Accept-Ranges: bytes\r\n"
            }
        };
        let longer;
        let body = if mode == RangeMode::IgnoredThenLonger && range.is_none() && earlier_plain > 0 {
            longer = [body, &[0u8; 16]].concat();
            &longer[..]
        } else {
            body
        };
        let requested = range.as_deref().and_then(|r| {
            let (a, b) = r.strip_prefix("bytes=")?.split_once('-')?;
            Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))
        });
        let (status, slice, content_range) = match requested {
            Some((start, end)) if mode == RangeMode::Supported => (
                "206 Partial Content",
                &body[start..=end],
                format!("Content-Range: bytes {start}-{end}/{}\r\n", body.len()),
            ),
            _ => ("200 OK", body, String::new()),
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{accept}{content_range}Connection: close\r\n\r\n",
            slice.len()
        );
        if stream.write_all(head.as_bytes()).is_err() {
            return;
        }
        let fast = requested
            .and_then(|(start, _)| collapse.and_then(|f| f(start)))
            .map_or(slice.len(), |n| n.min(slice.len()));
        let (fast, slow) = slice.split_at(fast);
        for piece in fast.chunks(16 * 1024) {
            // 用戶端中斷（取消、改單連線、換線）時寫入失敗，直接結束。
            if stream.write_all(piece).is_err() {
                return;
            }
            if !pause.is_zero() {
                std::thread::sleep(pause);
            }
        }
        for piece in slow.chunks(DRIP.0) {
            if stream.write_all(piece).is_err() {
                return;
            }
            std::thread::sleep(DRIP.1);
        }
    }

    /// 不重複的位元組序列，分段寫錯 offset 會被比對出來。
    fn sample_body(len: usize) -> Arc<Vec<u8>> {
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        Arc::new(
            (0..len)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    x as u8
                })
                .collect(),
        )
    }

    /// 超過分段門檻，且不被段數整除（最後一段較短）；也要夠大，慢速伺服器上的取消測試才不會在取消前就下完。
    const LARGE: usize = 8 * 1024 * 1024 + 123_457;

    /// 分段下載時第 `i` 段的範圍（含端點），與 [`download_ranges`] 的切法相同。
    fn segment(i: usize) -> (usize, usize) {
        let len = (LARGE as u64).div_ceil(PARALLEL_SEGMENTS) as usize;
        (i * len, ((i + 1) * len).min(LARGE) - 1)
    }

    /// `bytes=a-b` → `(a, b)`。
    fn range_of(header: &Option<String>) -> (usize, usize) {
        let (a, b) = header
            .as_deref()
            .and_then(|r| r.strip_prefix("bytes="))
            .and_then(|r| r.split_once('-'))
            .expect("range");
        (a.parse().expect("start"), b.parse().expect("end"))
    }

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client")
    }

    async fn fetch(url: &str, dest: &Path, cancel: &CancelToken) -> AppResult<u64> {
        download_to_file(&test_client(), url, dest, "測試", &|_| {}, cancel).await
    }

    async fn fetch_exact(url: &str, dest: &Path, length: u64) -> AppResult<u64> {
        let cancel = DownloadCancel::default();
        download_to_file_exact(&test_client(), url, dest, length, "測試", &|_| {}, &cancel.token())
            .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn large_file_is_fetched_as_parallel_ranges_byte_identical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = sample_body(LARGE);
        let stub = serve(Arc::clone(&body), RangeMode::Supported, Duration::ZERO);
        let dest = dir.path().join("out.bin");
        let cancel = DownloadCancel::default();

        let n = fetch(&stub.url, &dest, &cancel.token()).await.expect("download");

        assert_eq!(n, LARGE as u64);
        assert!(fs::read(&dest).expect("read") == *body, "內容不同");
        let ranges = stub.ranges();
        assert_eq!(ranges.len(), 1 + PARALLEL_SEGMENTS as usize);
        assert_eq!(ranges[0], None, "第一個請求只取標頭");
        // 各段首尾相接、涵蓋整個檔案。
        let mut segments: Vec<(usize, usize)> = ranges[1..].iter().map(range_of).collect();
        segments.sort_unstable();
        let mut next = 0;
        for (start, end) in segments {
            assert_eq!(start, next);
            next = end + 1;
        }
        assert_eq!(next, LARGE);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn single_connection_when_ranges_are_unusable_or_file_is_small() {
        let cancel = DownloadCancel::default();
        // (大小, 伺服器行為, 預期沒有 Range 的請求數)
        let cases = [
            (LARGE, RangeMode::Unsupported, 1),
            // 宣告支援卻回 200：分段全部作廢，整檔單連線重下。
            (LARGE, RangeMode::Ignored, 2),
            (PARALLEL_MIN_SIZE as usize - 1, RangeMode::Supported, 1),
        ];
        for (len, mode, plain_requests) in cases {
            let dir = tempfile::tempdir().expect("tempdir");
            let body = sample_body(len);
            let stub = serve(Arc::clone(&body), mode, Duration::ZERO);
            let dest = dir.path().join("out.bin");

            let n = fetch(&stub.url, &dest, &cancel.token()).await.expect("download");

            assert_eq!(n, len as u64, "{mode:?}");
            assert!(fs::read(&dest).expect("read") == *body, "{mode:?} 內容不同");
            let ranges = stub.ranges();
            assert_eq!(ranges.iter().filter(|r| r.is_none()).count(), plain_requests, "{mode:?}");
            if mode != RangeMode::Ignored {
                assert_eq!(ranges.len(), 1, "{mode:?} 應只有一個請求");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exact_length_downloads_on_every_path() {
        // 分段、單連線、分段作廢退回單連線。
        for mode in [RangeMode::Supported, RangeMode::Unsupported, RangeMode::Ignored] {
            let dir = tempfile::tempdir().expect("tempdir");
            let body = sample_body(LARGE);
            let stub = serve(Arc::clone(&body), mode, Duration::ZERO);
            let dest = dir.path().join("out.bin");

            let n = fetch_exact(&stub.url, &dest, LARGE as u64).await.expect("download");

            assert_eq!(n, LARGE as u64, "{mode:?}");
            assert!(fs::read(&dest).expect("read") == *body, "{mode:?} 內容不同");
        }
    }

    /// 首次回應走單連線（太小、或不支援 Range）時長度不符：不建立檔案，也不再送請求。
    #[tokio::test(flavor = "multi_thread")]
    async fn first_response_length_mismatch_is_rejected_before_creating_file() {
        let small = PARALLEL_MIN_SIZE as usize - 1;
        for (len, mode) in [(small, RangeMode::Supported), (LARGE, RangeMode::Unsupported)] {
            let dir = tempfile::tempdir().expect("tempdir");
            let stub = serve(sample_body(len), mode, Duration::ZERO);
            let dest = dir.path().join("out.bin");

            let result = fetch_exact(&stub.url, &dest, len as u64 - 1).await;

            assert!(matches!(result, Err(AppError::Protocol { .. })), "{mode:?}: {result:?}");
            assert!(!dest.exists(), "{mode:?} 長度不符時不建立檔案");
            assert_eq!(stub.ranges(), [None], "{mode:?}");
        }
    }

    /// 首次回應宣告支援 Range（分段的長度來源）但長度不符：在 `set_len` 前拒絕，不建立檔案、不送分段請求。
    #[tokio::test(flavor = "multi_thread")]
    async fn ranged_response_length_mismatch_is_rejected_before_set_len() {
        // 回應比預期長（配置會被放大）與比預期短。
        for expected in [PARALLEL_MIN_SIZE, LARGE as u64 + 1] {
            let dir = tempfile::tempdir().expect("tempdir");
            let stub = serve(sample_body(LARGE), RangeMode::Supported, Duration::ZERO);
            let dest = dir.path().join("out.bin");

            let result = fetch_exact(&stub.url, &dest, expected).await;

            assert!(matches!(result, Err(AppError::Protocol { .. })), "{expected}: {result:?}");
            assert!(!dest.exists(), "{expected}: 長度不符時不配置檔案");
            assert_eq!(stub.ranges(), [None], "{expected}: 不送分段請求");
        }
    }

    /// 分段作廢、退回單連線的重新 GET 長度不符：在覆寫或寫入前拒絕，檔案仍是分段時配置的預期長度。
    #[tokio::test(flavor = "multi_thread")]
    async fn fallback_response_length_mismatch_is_rejected_before_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = serve(sample_body(LARGE), RangeMode::IgnoredThenLonger, Duration::ZERO);
        let dest = dir.path().join("out.bin");

        let result = fetch_exact(&stub.url, &dest, LARGE as u64).await;

        assert!(matches!(result, Err(AppError::Protocol { .. })), "{result:?}");
        assert_eq!(
            fs::metadata(&dest).expect("dest").len(),
            LARGE as u64,
            "未以較長的回應覆寫或放大"
        );
        let plain = stub.ranges().iter().filter(|r| r.is_none()).count();
        assert_eq!(plain, 2, "有退回單連線重新 GET");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_stops_download_promptly_and_next_download_is_unaffected() {
        // 分段並行與單連線兩條路徑都要能取消。
        for mode in [RangeMode::Supported, RangeMode::Unsupported] {
            let dir = tempfile::tempdir().expect("tempdir");
            let paths = AppPaths::new(dir.path().to_path_buf());
            let committed = paths.dalamud_dir("goatcorp", "1.0.0");
            write(&committed.join(INJECTOR_EXE), b"old");
            let body = sample_body(LARGE);
            // 每條連線約 320 KB/s：單連線不取消要將近 30 秒。
            let slow = serve(Arc::clone(&body), mode, Duration::from_millis(50));
            let cancel = Arc::new(DownloadCancel::default());
            let cancelled_at = Arc::new(Mutex::new(None::<Instant>));
            let (trigger, at) = (Arc::clone(&cancel), Arc::clone(&cancelled_at));
            // 第一筆非零進度時取消，確保取消當下確實在下載中。
            let progress = move |p: Progress| {
                let mut at = at.lock().expect("lock");
                if p.done > 0 && at.is_none() {
                    *at = Some(Instant::now());
                    trigger.cancel();
                }
            };
            let staging = Staging::create(&paths, "test").expect("staging");
            let staging_dir = staging.path().to_path_buf();
            let dest = staging_dir.join("latest.zip");

            let result = download_to_file(
                &test_client(),
                &slow.url,
                &dest,
                "測試",
                &progress,
                &cancel.token(),
            )
            .await;

            assert!(matches!(result, Err(AppError::Cancelled)), "{mode:?}: {result:?}");
            let latency = cancelled_at.lock().expect("lock").expect("已觸發取消").elapsed();
            assert!(latency < Duration::from_secs(2), "{mode:?} 取消後 {latency:?} 才返回");
            drop(staging);
            assert!(!dest.exists(), "{mode:?} 下載檔仍在");
            assert!(!staging_dir.exists(), "{mode:?} 暫存仍在");
            assert_eq!(fs::read(committed.join(INJECTOR_EXE)).expect("committed"), b"old");

            // 同一個 DownloadCancel 的下一次下載不受上次取消影響。
            let fast = serve(Arc::clone(&body), mode, Duration::ZERO);
            let again = dir.path().join("again.zip");
            fetch(&fast.url, &again, &cancel.token()).await.expect("取消後再下載");
            assert!(fs::read(&again).expect("read") == *body, "{mode:?} 內容不同");
        }
    }

    /// 範圍落在 `(lo, hi)` 這一段內的請求（依送達順序）：該段的第一條連線與之後換線的連線。
    fn requests_in(ranges: &[Option<String>], (lo, hi): (usize, usize)) -> Vec<(usize, usize)> {
        ranges
            .iter()
            .filter(|r| r.is_some())
            .map(range_of)
            .filter(|&(start, end)| start >= lo && end == hi)
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn collapsed_connection_is_replaced_and_resumes_where_it_left_off() {
        // 崩落前送出的量，取實測下限 0.3 MB。
        const FAST: usize = 300 * 1024;
        let dir = tempfile::tempdir().expect("tempdir");
        let body = sample_body(LARGE);
        let collapsed = [segment(0), segment(3)];
        let starts = collapsed.map(|(start, _)| start);
        let stub = serve_collapsing(
            Arc::clone(&body),
            Arc::new(move |start: usize| starts.contains(&start).then_some(FAST)),
        );
        let dest = dir.path().join("out.bin");
        let cancel = DownloadCancel::default();

        let begun = Instant::now();
        let n = fetch(&stub.url, &dest, &cancel.token()).await.expect("download");
        let elapsed = begun.elapsed();

        assert_eq!(n, LARGE as u64);
        assert!(fs::read(&dest).expect("read") == *body, "內容不同");
        // 不換線的話，崩落的段剩下約 750 KB 要以 10 KB/s 下載 70 秒以上。
        assert!(elapsed < Duration::from_secs(10), "花了 {elapsed:?}");
        let ranges = stub.ranges();
        assert_eq!(ranges.len(), 1 + PARALLEL_SEGMENTS as usize + collapsed.len(), "{ranges:?}");
        for (start, end) in collapsed {
            let requests = requests_in(&ranges, (start, end));
            assert_eq!(requests.len(), 2, "{requests:?}");
            assert_eq!(requests[0], (start, end));
            // 新連線從已寫入處續抓到段尾，崩落前收到的不重下；接縫若留空隙，上面的內容比對會失敗。
            let (resume, resume_end) = requests[1];
            assert!(resume >= start + FAST && resume_end == end, "{requests:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn segment_that_never_recovers_fails_after_retry_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = sample_body(LARGE);
        let (lo, hi) = segment(5);
        // 這一段的每條連線（含換線後的）一開始就極慢，每次換線都沒有進展。
        let stub = serve_collapsing(
            Arc::clone(&body),
            Arc::new(move |start: usize| (lo..=hi).contains(&start).then_some(0)),
        );
        let dest = dir.path().join("out.bin");
        let cancel = DownloadCancel::default();

        let begun = Instant::now();
        let result = fetch(&stub.url, &dest, &cancel.token()).await;
        let elapsed = begun.elapsed();

        assert!(
            matches!(&result, Err(AppError::Network { message, .. }) if message.contains("過慢")),
            "{result:?}"
        );
        let attempts = requests_in(&stub.ranges(), (lo, hi)).len();
        assert_eq!(attempts, 1 + STALL_MAX_RETRIES as usize);
        // 每條連線一個時間窗就換掉，上限到了就停。
        assert!(elapsed < STALL_WINDOW * (attempts as u32 + 3), "花了 {elapsed:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_is_prompt_while_a_segment_is_switching_connections() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = sample_body(LARGE);
        let (lo, hi) = segment(5);
        let stub = serve_collapsing(
            Arc::clone(&body),
            Arc::new(move |start: usize| (lo..=hi).contains(&start).then_some(0)),
        );
        let dest = dir.path().join("out.bin");
        let cancel = Arc::new(DownloadCancel::default());
        // 該段出現第二條連線（已換線一次）就取消。
        let (log, trigger) = (Arc::clone(&stub.ranges), Arc::clone(&cancel));
        let watcher = tokio::spawn(async move {
            loop {
                let ranges = log.lock().expect("lock").clone();
                if requests_in(&ranges, (lo, hi)).len() >= 2 {
                    trigger.cancel();
                    return Instant::now();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        let result = fetch(&stub.url, &dest, &cancel.token()).await;
        let latency = watcher.await.expect("watcher").elapsed();

        assert!(matches!(result, Err(AppError::Cancelled)), "{result:?}");
        assert!(latency < Duration::from_secs(2), "取消後 {latency:?} 才返回");
    }
}
