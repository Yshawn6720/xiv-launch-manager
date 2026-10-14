//! 國際服完整性檢查（protocols §5.3）：以 goatcorp 的整檔 SHA1 對照逐檔核對安裝目錄。
//!
//! 唯讀：只以唯讀 handle 開檔，不建立、不修改任何檔案。對照資料由 goatcorp 第三方維護，
//! 只用於回報；key 一律先檢查，任何一條可能指到安裝根目錄外就拒絕整份對照。
//! 只處理國際服：台服沒有對照來源，`check_game_integrity` command 只對國際服安裝呼叫。

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use tokio::sync::watch;

use crate::dalamud::{CancelToken, Progress, ProgressFn, parse_json, safe_relative_path};
use crate::error::{AppError, AppResult};
use crate::patch::list::is_version_id;

const REFERENCE_BASE_URL: &str = "https://goatcorp.github.io/integrity";
const STAGE: &str = "完整性檢查";
const FETCH_CONTEXT: &str = "下載完整性對照";
const REFERENCE_CONTEXT: &str = "完整性對照";
const READ_BUF_SIZE: usize = 64 * 1024;
/// 進度回報的最小間隔（bytes）。遊戲檔總量數十 GB，間隔太小會讓前端事件過多。
const PROGRESS_STEP: u64 = 8 * 1024 * 1024;

/// 一次完整性檢查的結果（`check_game_integrity` 回前端）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum IntegrityOutcome {
    Checked(IntegrityReport),
    /// 對照來源回 404：該版本沒有對照資料（不是錯誤）。
    NoReference { game_version: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrityReport {
    /// 本機 `game\ffxivgame.ver`，已確認等於對照的 `GameVersion`。
    pub game_version: String,
    /// 不存在或不是一般檔案的項目，內容為對照原樣的 key（如 `\game\ffxiv_dx11.exe`），依 key 排序。
    pub missing: Vec<String>,
    /// 整檔 SHA1 與對照不符的項目，格式同 `missing`。
    pub mismatched: Vec<String>,
    /// 對照列出的項目數（＝相符＋`missing`＋`mismatched`）。
    pub checked: usize,
}

/// 下載 `{ffxivgame.ver}.json` 對照並逐檔核對 `install` 下的檔案。`.ver` 缺失或格式不符即錯誤
/// （不以基礎版本兜底）；對照的 `GameVersion` 與本機不符、key 不安全或 hash 格式錯都拒絕整份對照。
/// 進度以 bytes 計（`total`＝對照列出且存在的檔案大小總和）；取消在網路等待中立即生效，
/// 核對中每讀一段檢查一次。
pub async fn check_integrity(
    client: &reqwest::Client,
    install: &Path,
    progress: &ProgressFn,
    cancel: &CancelToken,
) -> AppResult<IntegrityOutcome> {
    check_against(client, REFERENCE_BASE_URL, install, progress, cancel).await
}

async fn check_against(
    client: &reqwest::Client,
    base_url: &str,
    install: &Path,
    progress: &ProgressFn,
    cancel: &CancelToken,
) -> AppResult<IntegrityOutcome> {
    cancel.check()?;
    let game_version = read_game_version(install)?;
    let url = format!("{base_url}/{game_version}.json");
    let Some(body) = fetch_reference(client, &url, cancel).await? else {
        return Ok(IntegrityOutcome::NoReference { game_version });
    };
    let targets = parse_reference(&body, &game_version, install)?;
    let checked = targets.len();

    let (tx, mut rx) = watch::channel((0u64, 0u64));
    let token = cancel.clone();
    let job = tokio::task::spawn_blocking(move || {
        let mut reported = 0u64;
        scan(&targets, &token, &mut |done, total| {
            if done == 0 || done == total || done - reported >= PROGRESS_STEP {
                reported = done;
                tx.send_replace((done, total));
            }
        })
    });
    // 發送端隨 blocking 工作結束而 drop，迴圈在最後一筆進度送出後結束。
    while rx.changed().await.is_ok() {
        let (done, total) = *rx.borrow_and_update();
        progress(Progress {
            stage: STAGE.to_string(),
            done,
            total: Some(total),
        });
    }
    let scanned = job.await.map_err(|e| AppError::Io {
        context: STAGE.to_string(),
        message: e.to_string(),
    })??;
    Ok(IntegrityOutcome::Checked(IntegrityReport {
        game_version,
        missing: scanned.missing,
        mismatched: scanned.mismatched,
        checked,
    }))
}

/// `{install}\game\ffxivgame.ver`，trim 後須為 `YYYY.MM.DD.NNNN.NNNN`（會放進 URL path）。
fn read_game_version(install: &Path) -> AppResult<String> {
    let path = install.join("game").join("ffxivgame.ver");
    let text = fs::read_to_string(&path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => AppError::GameFileMissing {
            path: path.display().to_string(),
        },
        _ => AppError::io(format!("讀取 {}", path.display()), e),
    })?;
    let version = text.trim();
    if !is_version_id(version) {
        return Err(AppError::Io {
            context: format!("讀取 {}", path.display()),
            message: "版本格式不符".to_string(),
        });
    }
    Ok(version.to_string())
}

/// 回 `None`＝404（該版本沒有對照）；其他非 2xx 為網路錯誤。
async fn fetch_reference(
    client: &reqwest::Client,
    url: &str,
    cancel: &CancelToken,
) -> AppResult<Option<Vec<u8>>> {
    cancel
        .race(async {
            let response = client
                .get(url)
                .send()
                .await
                .map_err(|e| AppError::network(FETCH_CONTEXT, e))?;
            if response.status() == StatusCode::NOT_FOUND {
                return Ok(None);
            }
            let body = response
                .error_for_status()
                .map_err(|e| AppError::network(FETCH_CONTEXT, e))?
                .bytes()
                .await
                .map_err(|e| AppError::network(FETCH_CONTEXT, e))?;
            Ok(Some(Vec::from(body)))
        })
        .await
}

/// 對照 JSON（`LastGameVersion` 不使用）。
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Reference {
    game_version: String,
    hashes: BTreeMap<String, String>,
}

/// 對照中的一個檔案，key 與 hash 都已通過檢查。
struct Target {
    key: String,
    path: PathBuf,
    expected: [u8; 20],
}

/// 任一項不合即拒絕整份對照，不回傳部分結果。空的 `Hashes` 會讓檢查無條件通過，也拒絕。
fn parse_reference(bytes: &[u8], game_version: &str, install: &Path) -> AppResult<Vec<Target>> {
    let reference: Reference = parse_json(bytes, REFERENCE_CONTEXT)?;
    if reference.game_version != game_version {
        return Err(AppError::protocol("完整性對照的版本與本機不符"));
    }
    if reference.hashes.is_empty() {
        return Err(AppError::protocol("完整性對照沒有任何檔案"));
    }
    reference
        .hashes
        .into_iter()
        .map(|(key, hash)| -> AppResult<Target> {
            let path = key_path(install, &key)
                .ok_or_else(|| AppError::protocol("完整性對照含不安全的路徑"))?;
            let expected =
                parse_hash(&hash).ok_or_else(|| AppError::protocol("完整性對照的 hash 格式"))?;
            Ok(Target {
                key,
                path,
                expected,
            })
        })
        .collect()
}

/// key 去除前導分隔符後相對安裝根目錄定位（不當 rooted path join，否則會落到磁碟根）。
/// `..`、磁碟代號、`:`（含 NTFS 資料流）由 [`safe_relative_path`] 拒絕；Windows 檔名不允許的
/// 字元與控制字元也拒絕（如 `\\?\` 前綴去掉分隔符後剩下的 `?`）。
fn key_path(install: &Path, key: &str) -> Option<PathBuf> {
    let rest = key.trim_start_matches(['\\', '/']);
    if rest
        .chars()
        .any(|c| c.is_control() || matches!(c, '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return None;
    }
    safe_relative_path(rest).map(|rel| install.join(rel))
}

/// `AA BB …`：20 個以單一空白分隔的兩位大寫 hex（XL `IntegrityCheck.cs` 的輸出格式）。
fn parse_hash(text: &str) -> Option<[u8; 20]> {
    let mut out = [0u8; 20];
    let mut parts = text.split(' ');
    for byte in &mut out {
        let part = parts.next()?;
        if part.len() != 2
            || !part
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
        {
            return None;
        }
        *byte = u8::from_str_radix(part, 16).ok()?;
    }
    parts.next().is_none().then_some(out)
}

#[derive(Default)]
struct Scan {
    missing: Vec<String>,
    mismatched: Vec<String>,
}

/// 先量出存在檔案的總大小，再逐檔串流計算 SHA1。`on_progress(done, total)` 每讀一段呼叫一次。
fn scan(
    targets: &[Target],
    cancel: &CancelToken,
    on_progress: &mut dyn FnMut(u64, u64),
) -> AppResult<Scan> {
    let mut sizes = Vec::with_capacity(targets.len());
    for target in targets {
        cancel.check()?;
        sizes.push(regular_file_len(&target.path).map_err(|e| read_error(&target.key, e))?);
    }
    let total: u64 = sizes.iter().flatten().sum();
    let mut done = 0u64;
    on_progress(done, total);
    let mut buf = vec![0u8; READ_BUF_SIZE];
    let mut scanned = Scan::default();
    for (target, size) in targets.iter().zip(sizes) {
        cancel.check()?;
        if size.is_none() {
            scanned.missing.push(target.key.clone());
            continue;
        }
        let actual = file_sha1(target, &mut buf, cancel, &mut |n| {
            done += n;
            on_progress(done, total);
        })?;
        match actual {
            None => scanned.missing.push(target.key.clone()),
            Some(actual) if actual != target.expected => {
                scanned.mismatched.push(target.key.clone());
            }
            Some(_) => {}
        }
    }
    Ok(scanned)
}

/// 一般檔案的大小；不存在或不是一般檔案（目錄、裝置）為 `None`。
fn regular_file_len(path: &Path) -> io::Result<Option<u64>> {
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() => Ok(Some(meta.len())),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// 唯讀開啟並串流計算整檔 SHA1；`None`＝開檔時已不存在或不是一般檔案。
fn file_sha1(
    target: &Target,
    buf: &mut [u8],
    cancel: &CancelToken,
    on_read: &mut dyn FnMut(u64),
) -> AppResult<Option<[u8; 20]>> {
    let mut file = match File::open(&target.path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(read_error(&target.key, e)),
    };
    if !file
        .metadata()
        .map_err(|e| read_error(&target.key, e))?
        .is_file()
    {
        return Ok(None);
    }
    let mut hasher = Sha1::new();
    loop {
        cancel.check()?;
        let n = match file.read(buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(read_error(&target.key, e)),
        };
        hasher.update(&buf[..n]);
        on_read(n as u64);
    }
    Ok(Some(hasher.finalize().into()))
}

fn read_error(key: &str, e: io::Error) -> AppError {
    AppError::io(format!("讀取 {key}"), e)
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime};

    use crate::dalamud::DownloadCancel;

    use super::*;

    const VERSION: &str = "2026.09.15.0000.0000";
    const EXE_KEY: &str = "\\game\\ffxiv_dx11.exe";
    const DAT_KEY: &str = "\\game\\sqpack\\ffxiv\\000000.win32.dat0";
    const EXE: &[u8] = b"ffxiv_dx11";

    /// 跨多個讀取段（64 KiB）且不整除。
    fn dat() -> Vec<u8> {
        (0..300 * 1024 + 7).map(|i| (i * 31 % 251) as u8).collect()
    }

    fn write(path: &Path, data: &[u8]) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, data).expect("write");
    }

    /// 假安裝：`game\ffxivgame.ver`（CRLF，與實際檔案相同）、exe 與一個 dat。
    fn fake_install() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &dir.path().join("game/ffxivgame.ver"),
            format!("{VERSION}\r\n").as_bytes(),
        );
        write(&dir.path().join("game/ffxiv_dx11.exe"), EXE);
        write(&dir.path().join("game/sqpack/ffxiv/000000.win32.dat0"), &dat());
        dir
    }

    /// 上游格式：大寫、位元組間以空白分隔。
    fn spaced_sha1(data: &[u8]) -> String {
        let hex = hex::encode_upper(Sha1::digest(data));
        hex.as_bytes()
            .chunks(2)
            .map(|pair| std::str::from_utf8(pair).expect("hex"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn reference(version: &str, hashes: &[(&str, String)]) -> Vec<u8> {
        let hashes: serde_json::Map<String, serde_json::Value> = hashes
            .iter()
            .map(|(key, hash)| (key.to_string(), serde_json::Value::from(hash.as_str())))
            .collect();
        serde_json::to_vec(&serde_json::json!({
            "GameVersion": version,
            "LastGameVersion": "2026.08.07.0000.0000",
            "Hashes": hashes,
        }))
        .expect("json")
    }

    fn matching_hashes() -> Vec<(&'static str, String)> {
        vec![(EXE_KEY, spaced_sha1(EXE)), (DAT_KEY, spaced_sha1(&dat()))]
    }

    type Snapshot = BTreeMap<PathBuf, (Vec<u8>, SystemTime)>;

    /// 整棵目錄每個檔案的內容與 mtime；同時可看出是否多出或少了檔案。
    fn snapshot(root: &Path) -> Snapshot {
        let mut out = Snapshot::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).expect("read_dir") {
                let path = entry.expect("entry").path();
                let meta = fs::metadata(&path).expect("metadata");
                if meta.is_dir() {
                    stack.push(path);
                } else {
                    let content = fs::read(&path).expect("read");
                    out.insert(path, (content, meta.modified().expect("mtime")));
                }
            }
        }
        out
    }

    /// 本機 HTTP/1.1 測試伺服器（同 `dalamud::install` 測試）：每個請求回固定狀態與 body，
    /// 回應前先等 `delay`；`paths` 依序記錄請求路徑。
    struct Stub {
        base: String,
        paths: Arc<Mutex<Vec<String>>>,
    }

    impl Stub {
        fn paths(&self) -> Vec<String> {
            self.paths.lock().expect("lock").clone()
        }
    }

    fn serve(status: &'static str, body: Vec<u8>, delay: Duration) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}/integrity", listener.local_addr().expect("addr"));
        let paths = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&paths);
        let body = Arc::new(body);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (body, log) = (Arc::clone(&body), Arc::clone(&log));
                std::thread::spawn(move || respond(stream, status, &body, delay, &log));
            }
        });
        Stub { base, paths }
    }

    fn respond(
        mut stream: TcpStream,
        status: &str,
        body: &[u8],
        delay: Duration,
        log: &Mutex<Vec<String>>,
    ) {
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            if line.trim_end().is_empty() {
                break;
            }
        }
        if let Some(path) = request_line.split_whitespace().nth(1) {
            log.lock().expect("lock").push(path.to_string());
        }
        std::thread::sleep(delay);
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        // 用戶端已中斷（取消）時寫入失敗，直接結束。
        let _ = stream
            .write_all(head.as_bytes())
            .and_then(|()| stream.write_all(body));
    }

    fn serve_reference(body: Vec<u8>) -> Stub {
        serve("200 OK", body, Duration::ZERO)
    }

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client")
    }

    async fn run(stub: &Stub, install: &Path) -> AppResult<IntegrityOutcome> {
        let cancel = DownloadCancel::default();
        check_against(&test_client(), &stub.base, install, &|_| {}, &cancel.token()).await
    }

    fn report_of(missing: &[&str], mismatched: &[&str], checked: usize) -> IntegrityOutcome {
        IntegrityOutcome::Checked(IntegrityReport {
            game_version: VERSION.to_string(),
            missing: missing.iter().map(|s| s.to_string()).collect(),
            mismatched: mismatched.iter().map(|s| s.to_string()).collect(),
            checked,
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn all_files_match_and_nothing_is_modified() {
        let dir = fake_install();
        let before = snapshot(dir.path());
        let stub = serve_reference(reference(VERSION, &matching_hashes()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&events);
        let progress = move |p: Progress| log.lock().expect("lock").push((p.done, p.total));
        let cancel = DownloadCancel::default();

        let outcome = check_against(
            &test_client(),
            &stub.base,
            dir.path(),
            &progress,
            &cancel.token(),
        )
        .await
        .expect("check");

        assert_eq!(outcome, report_of(&[], &[], 2));
        assert_eq!(stub.paths(), [format!("/integrity/{VERSION}.json")]);
        let total = (EXE.len() + dat().len()) as u64;
        let events = events.lock().expect("lock").clone();
        assert_eq!(events.last(), Some(&(total, Some(total))), "{events:?}");
        assert_eq!(snapshot(dir.path()), before);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_files_are_reported() {
        let dir = fake_install();
        let before = snapshot(dir.path());
        let mut hashes = matching_hashes();
        let absent = "\\game\\sqpack\\ffxiv\\000001.win32.dat0";
        // 目錄不是一般檔案，也算缺檔。
        let directory = "\\game\\sqpack\\ffxiv";
        hashes.push((absent, spaced_sha1(b"absent")));
        hashes.push((directory, spaced_sha1(b"dir")));
        let stub = serve_reference(reference(VERSION, &hashes));

        let outcome = run(&stub, dir.path()).await.expect("check");

        assert_eq!(outcome, report_of(&[directory, absent], &[], 4));
        assert_eq!(snapshot(dir.path()), before);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mismatched_files_are_reported() {
        let dir = fake_install();
        let before = snapshot(dir.path());
        let hashes = [
            (EXE_KEY, spaced_sha1(b"original exe")),
            (DAT_KEY, spaced_sha1(&dat())),
        ];
        let stub = serve_reference(reference(VERSION, &hashes));

        let outcome = run(&stub, dir.path()).await.expect("check");

        assert_eq!(outcome, report_of(&[], &[EXE_KEY], 2));
        assert_eq!(snapshot(dir.path()), before);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn not_found_means_no_reference() {
        let dir = fake_install();
        let before = snapshot(dir.path());
        let stub = serve("404 Not Found", b"not found".to_vec(), Duration::ZERO);

        let outcome = run(&stub, dir.path()).await.expect("check");

        assert_eq!(
            outcome,
            IntegrityOutcome::NoReference {
                game_version: VERSION.to_string()
            }
        );
        assert_eq!(snapshot(dir.path()), before);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reference_for_another_version_is_rejected() {
        let dir = fake_install();
        let stub = serve_reference(reference("2026.09.14.0000.0000", &matching_hashes()));

        let result = run(&stub, dir.path()).await;

        assert!(matches!(result, Err(AppError::Protocol { .. })), "{result:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn escaping_keys_reject_the_whole_reference() {
        let dir = fake_install();
        let before = snapshot(dir.path());
        for bad in [
            "\\..\\outside.txt",
            "\\game\\..\\..\\outside.txt",
            "/game/../../outside.txt",
            "\\game\\.. \\outside.txt",
            "C:\\Windows\\win.ini",
            "\\\\?\\C:\\Windows\\win.ini",
            "\\\\?\\UNC\\server\\share\\x",
            "\\game\\ffxiv_dx11.exe:stream",
            "\\game\\a\0b",
            "\\",
        ] {
            let mut hashes = matching_hashes();
            hashes.push((bad, spaced_sha1(b"x")));
            let stub = serve_reference(reference(VERSION, &hashes));

            let result = run(&stub, dir.path()).await;

            assert!(
                matches!(result, Err(AppError::Protocol { .. })),
                "{bad:?}: {result:?}"
            );
        }
        assert_eq!(snapshot(dir.path()), before);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn malformed_hash_rejects_the_whole_reference() {
        let dir = fake_install();
        let good = spaced_sha1(EXE);
        let bad_hashes = [
            // 小寫、無分隔、分隔符不對、多餘空白。
            good.to_lowercase(),
            good.replace(' ', ""),
            good.replace(' ', "-"),
            good.replace(' ', "  "),
            format!("{good} "),
            format!(" {good}"),
            // 21、19 個位元組，最後一個三位數，非 hex，空字串。
            format!("{good} AA"),
            good[3..].to_string(),
            format!("{good}A"),
            format!("ZZ{}", &good[2..]),
            String::new(),
        ];
        for bad in bad_hashes {
            let hashes = [(EXE_KEY, bad.clone()), (DAT_KEY, spaced_sha1(&dat()))];
            let stub = serve_reference(reference(VERSION, &hashes));

            let result = run(&stub, dir.path()).await;

            assert!(
                matches!(result, Err(AppError::Protocol { .. })),
                "{bad:?}: {result:?}"
            );
        }
        // 空表會讓檢查無條件通過。
        let stub = serve_reference(reference(VERSION, &[]));
        assert!(matches!(
            run(&stub, dir.path()).await,
            Err(AppError::Protocol { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_or_malformed_ver_is_an_error_without_request() {
        let dir = fake_install();
        let stub = serve_reference(reference(VERSION, &matching_hashes()));
        let ver = dir.path().join("game/ffxivgame.ver");

        fs::remove_file(&ver).expect("remove ver");
        let result = run(&stub, dir.path()).await;
        assert!(
            matches!(result, Err(AppError::GameFileMissing { .. })),
            "{result:?}"
        );

        fs::write(&ver, "../2026.09.15.0000.0000").expect("write ver");
        let result = run(&stub, dir.path()).await;
        assert!(matches!(result, Err(AppError::Io { .. })), "{result:?}");

        assert!(stub.paths().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_while_fetching_returns_promptly() {
        let dir = fake_install();
        let stub = serve(
            "200 OK",
            reference(VERSION, &matching_hashes()),
            Duration::from_secs(10),
        );
        let cancel = Arc::new(DownloadCancel::default());
        let token = cancel.token();
        let trigger = Arc::clone(&cancel);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            trigger.cancel();
        });
        let started = Instant::now();

        let result = check_against(&test_client(), &stub.base, dir.path(), &|_| {}, &token).await;

        assert!(matches!(result, Err(AppError::Cancelled)), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn cancel_while_hashing_stops_the_scan() {
        let dir = fake_install();
        let before = snapshot(dir.path());
        let targets = parse_reference(
            &reference(VERSION, &matching_hashes()),
            VERSION,
            dir.path(),
        )
        .expect("reference");
        let cancel = DownloadCancel::default();
        let token = cancel.token();
        let mut last = (0, 0);

        // 第一段讀完就取消：下一段之前就要停下，不會讀完全部。
        let result = scan(&targets, &token, &mut |done, total| {
            last = (done, total);
            if done > 0 {
                cancel.cancel();
            }
        });

        assert!(matches!(result, Err(AppError::Cancelled)), "{:?}", result.err());
        let (done, total) = last;
        assert!(done > 0 && done < total, "{done}/{total}");
        assert_eq!(snapshot(dir.path()), before);
    }
}
