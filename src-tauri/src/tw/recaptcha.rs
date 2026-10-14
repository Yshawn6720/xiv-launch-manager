//! 繁中服登入確認對話框與 reCAPTCHA token（protocols §2.1 第 1 步；主 note「台服 reCAPTCHA」）。
//!
//! 流程：開 `tw-recaptcha` 視窗（about:blank，置中、取得焦點）→ 在該 WebView2 攔截
//! `https://launcher.ffxiv.com.tw/index.html` 改回內建頁（填入帳號名稱、是否在頁面輸入 OTP）→ 確認攔截生效才導航 →
//! 頁面載入 reCAPTCHA，就緒後才能按「登入」→ 使用者按下時頁面才 execute（同 TC 與官方啟動器）→ token（手動 OTP
//! 帳號連同剛輸入的 OTP）經 `tw_recaptcha_event` 交回 → 呼叫端拿著 [`LoginDialog`] 送登入，期間頁面顯示
//! 「登入中…」、視窗不能關 → [`LoginDialog`] drop 時關窗（同 TC：登入流程跑完才關）。
//!
//! 逾時：設定攔截 [`SETUP_TIMEOUT`]；導航到頁面就緒 [`READY_TIMEOUT`]（頁面或 reCAPTCHA 載不起來）；等使用者按
//! 「登入」不設期限（對話框一直可見，可按「取消」或關窗）；按下到拿到 token [`TOKEN_TIMEOUT`]。
//! 按「取消」或在交回 token 前關窗都回 [`AppError::RecaptchaCancelled`]。
//!
//! 不變量：
//! - 攔截沒設定成功就不導航，真站內容不會進到這個有 IPC 權限的視窗。
//! - 頂層文件只能是 about:blank 或上述 URL。capability 的 remote URL 對 fetch IPC
//!   只比得到 origin（Tauri 以 `Origin` header 判定），path 由 `on_navigation` 釘死。
//! - 同一時間最多一個對話框；token、OTP 不進錯誤、log 或主視窗。密碼不進頁面。

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::Deserialize;
use tauri::webview::PlatformWebview;
use tauri::{
    AppHandle, Manager, State, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent,
};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, timeout, timeout_at};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL, ICoreWebView2Environment,
    ICoreWebView2WebResourceRequestedEventArgs, ICoreWebView2WebResourceResponse,
};
use webview2_com::{WebResourceRequestedEventHandler, take_pwstr};
use windows::Win32::Foundation::E_OUTOFMEMORY;
use windows::Win32::UI::Shell::SHCreateMemStream;
use windows::core::{HSTRING, PWSTR};

use crate::error::{AppError, AppResult};

const WINDOW_LABEL: &str = "tw-recaptcha";
const WINDOW_TITLE: &str = "繁中服登入確認";
/// 帳號名稱、說明、（手動 OTP 時）OTP 欄、狀態列與「取消」「登入」，下方留給 reCAPTCHA 徽章。
const WINDOW_WIDTH: f64 = 460.0;
const WINDOW_HEIGHT: f64 = 340.0;
const WINDOW_HEIGHT_WITH_OTP: f64 = 390.0;
const BLANK_URL: &str = "about:blank";
/// reCAPTCHA Enterprise 只接受這個 origin；請求一律被攔截，不會連到真站。
const PAGE_URL: &str = "https://launcher.ffxiv.com.tw/index.html";
const PAGE_HTML: &str = include_str!("recaptcha.html");
const PAGE_HEADERS: &str = "Content-Type: text/html; charset=utf-8";

const SETUP_TIMEOUT: Duration = Duration::from_secs(15);
/// 導航到頁面回報 reCAPTCHA 就緒。
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// 按下「登入」到拿到 token；execute 平常 1 秒內完成，token 本身 2 分鐘內有效。
const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);
/// token 長度上限；超過視為異常輸入。
const MAX_TOKEN_LEN: usize = 4096;

/// 對話框與 [`tw_recaptcha_event`] 的交接點。Main 以 `.manage(RecaptchaState::default())` 註冊。
#[derive(Default)]
pub struct RecaptchaState {
    slot: Arc<Mutex<Slot>>,
}

#[derive(Default)]
struct Slot {
    /// 每次 acquire 遞增；舊視窗遲到的 Destroyed 事件不得清掉新的對話框。
    generation: u64,
    phase: Phase,
}

#[derive(Default)]
enum Phase {
    #[default]
    Idle,
    /// 對話框開著，頁面事件轉給等待中的 [`acquire_token`]。
    Waiting(mpsc::UnboundedSender<PageEvent>),
    /// token 已交回、登入進行中：之後的頁面事件一律丟棄；關窗撤回不了已送出的登入，所以不讓關，
    /// 由 [`LoginDialog`] drop 時關。
    LoggingIn,
}

/// 頁面經 [`tw_recaptcha_event`] 送來的事件。不 derive `Debug`：`Token` 帶 token 與 OTP。
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PageEvent {
    /// reCAPTCHA 就緒，「登入」可按。
    Ready,
    /// 使用者按下「登入」，頁面隨即 execute。
    Submit,
    /// execute 的結果；手動 OTP 帳號帶使用者在頁面輸入的 OTP。
    Token { token: String, otp: Option<String> },
    /// 使用者按「取消」或 Esc。
    Cancel,
    /// 頁面無法完成驗證。
    Failed { reason: PageFailure },
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PageFailure {
    /// reCAPTCHA 腳本載不起來。
    Load,
    /// execute 丟例外或被拒。
    Execute,
    /// execute 回了空值。
    EmptyToken,
}

impl PageFailure {
    fn message(self) -> &'static str {
        match self {
            PageFailure::Load => "無法載入 reCAPTCHA，請檢查網路後重試",
            PageFailure::Execute => "驗證失敗，請稍後重試",
            PageFailure::EmptyToken => "沒有回傳驗證結果，請稍後重試",
        }
    }
}

fn lock(slot: &Mutex<Slot>) -> MutexGuard<'_, Slot> {
    // 臨界區只換 phase，panic 不會留下半更新的狀態，poison 後可直接沿用。
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 開始一次等待；已有對話框時回 `None`，不搶走另一次 acquire 的事件。
fn begin(slot: &Mutex<Slot>) -> Option<(u64, mpsc::UnboundedReceiver<PageEvent>)> {
    let mut slot = lock(slot);
    if !matches!(slot.phase, Phase::Idle) {
        return None;
    }
    let (sender, receiver) = mpsc::unbounded_channel();
    slot.generation = slot.generation.wrapping_add(1);
    slot.phase = Phase::Waiting(sender);
    Some((slot.generation, receiver))
}

/// 結束指定那一次；還在等事件時丟掉 sender，接收端得到 `None`（＝視窗已關）。
fn end(slot: &Mutex<Slot>, generation: u64) {
    let mut slot = lock(slot);
    if slot.generation == generation {
        slot.phase = Phase::Idle;
    }
}

fn is_logging_in(slot: &Mutex<Slot>, generation: u64) -> bool {
    let slot = lock(slot);
    slot.generation == generation && matches!(slot.phase, Phase::LoggingIn)
}

/// 頁面事件轉給等待者。token 一交出去就轉成登入中：之後的事件（例如遲到的取消）不再影響這次登入。
fn deliver(slot: &Mutex<Slot>, event: PageEvent) {
    let mut slot = lock(slot);
    let Phase::Waiting(sender) = &slot.phase else {
        return; // 沒有等待者（已取消、逾時或已交回 token）就丟棄
    };
    let hands_over_token = matches!(event, PageEvent::Token { .. });
    let _ = sender.send(event); // 失敗代表等待端剛好結束，事件隨之丟棄即可
    if hands_over_token {
        slot.phase = Phase::LoggingIn;
    }
}

/// 一次 acquire 的資源；任何離開路徑（含 future 被丟棄）都結束這次對話框並關窗。
struct Attempt {
    slot: Arc<Mutex<Slot>>,
    generation: u64,
    window: Option<WebviewWindow>,
}

impl Drop for Attempt {
    fn drop(&mut self) {
        end(&self.slot, self.generation);
        if let Some(window) = self.window.take() {
            let _ = window.destroy(); // 失敗只代表視窗已不在（例如使用者已關閉），沒有殘留要清
        }
    }
}

/// 使用者按下「登入」後交回的 token 與手動 OTP。持有期間對話框顯示「登入中…」、不能關；drop 時關窗，
/// 呼叫端在登入流程結束後才 drop（同 TC）。不 derive `Debug`：含 token 與 OTP。
pub struct LoginDialog {
    token: String,
    otp: Option<String>,
    _attempt: Attempt,
}

impl LoginDialog {
    pub fn token(&self) -> &str {
        &self.token
    }

    /// 手動 OTP 帳號在對話框輸入的 OTP；其他帳號為 `None`。
    pub fn typed_otp(&self) -> Option<&str> {
        self.otp.as_deref()
    }
}

/// 開登入確認對話框，等使用者按「登入」取 reCAPTCHA token。`account` 顯示在對話框上；`otp_in_dialog`＝
/// 手動 OTP 帳號，頁面多一個 OTP 欄，填好 6 碼才能按「登入」。取消或關窗 → `RecaptchaCancelled`；
/// 逾時或頁面回報失敗 → `Recaptcha`。
pub async fn acquire_token(app: &AppHandle, account: &str, otp_in_dialog: bool) -> AppResult<LoginDialog> {
    let slot = match app.try_state::<RecaptchaState>() {
        Some(state) => Arc::clone(&state.slot),
        None => return Err(recaptcha_error("RecaptchaState 未註冊")),
    };
    let Some((generation, events)) = begin(&slot) else {
        return Err(recaptcha_error("已有進行中的登入確認視窗"));
    };
    let mut attempt = Attempt {
        slot,
        generation,
        window: None,
    };

    let window = open_window(app, otp_in_dialog)?;
    let event_slot = Arc::clone(&attempt.slot);
    window.on_window_event(move |event| match event {
        WindowEvent::CloseRequested { api, .. } if is_logging_in(&event_slot, generation) => api.prevent_close(),
        WindowEvent::Destroyed => end(&event_slot, generation),
        _ => {}
    });
    attempt.window = Some(window.clone());

    install_interceptor(&window, render_page(account, otp_in_dialog)).await?;
    let page = parse_url(PAGE_URL)?;
    window
        .navigate(page)
        .map_err(|e| recaptcha_error(format!("無法載入登入確認頁：{e}")))?;
    let _ = window.set_focus(); // 搶不到焦點（Windows 前景限制）時視窗仍可見，工作列會閃爍提示

    let granted = wait_for_token(events, otp_in_dialog, &DEFAULT_TIMEOUTS).await?;
    Ok(LoginDialog {
        token: granted.token,
        otp: granted.otp,
        _attempt: attempt,
    })
}

/// 頁面交回的 token 與手動 OTP。不 derive `Debug`。
struct Granted {
    token: String,
    otp: Option<String>,
}

struct Timeouts {
    ready: Duration,
    token: Duration,
}

const DEFAULT_TIMEOUTS: Timeouts = Timeouts {
    ready: READY_TIMEOUT,
    token: TOKEN_TIMEOUT,
};

/// 依頁面事件推進，直到拿到 token、取消、失敗或逾時。`events` 收到 `None`＝視窗已關（Destroyed 清掉了 sender）。
async fn wait_for_token(
    mut events: mpsc::UnboundedReceiver<PageEvent>,
    otp_in_dialog: bool,
    timeouts: &Timeouts,
) -> AppResult<Granted> {
    // 先等頁面就緒；就緒後等使用者按「登入」不設期限；按下後要在期限內拿到 token。
    let mut deadline = Some((Instant::now() + timeouts.ready, "登入確認頁載入逾時"));
    let mut submitted = false;
    loop {
        let event = match deadline {
            Some((at, what)) => timeout_at(at, events.recv())
                .await
                .map_err(|_| recaptcha_error(what))?,
            None => events.recv().await,
        };
        match event {
            None | Some(PageEvent::Cancel) => return Err(AppError::RecaptchaCancelled),
            Some(PageEvent::Ready) => {
                // IPC 不保證先後：按下之後才到的 ready 不得取消 token 的期限。
                if !submitted {
                    deadline = None;
                }
            }
            Some(PageEvent::Submit) => {
                submitted = true;
                deadline = Some((Instant::now() + timeouts.token, "取得驗證結果逾時"));
            }
            Some(PageEvent::Token { token, otp }) => {
                return granted(token, otp, otp_in_dialog);
            }
            Some(PageEvent::Failed { reason }) => return Err(recaptcha_error(reason.message())),
        }
    }
}

/// 檢查頁面交回的值。頁面已擋下空 token 與非 6 位數字的 OTP，這裡不符代表頁面異常。
fn granted(token: String, otp: Option<String>, otp_in_dialog: bool) -> AppResult<Granted> {
    if token.is_empty() || token.len() > MAX_TOKEN_LEN {
        return Err(recaptcha_error("沒有回傳有效的驗證結果"));
    }
    let otp = if otp_in_dialog {
        match otp {
            Some(code) if code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()) => Some(code),
            _ => return Err(recaptcha_error("登入確認視窗交回的 OTP 碼格式不符")),
        }
    } else {
        None
    };
    Ok(Granted { token, otp })
}

/// 登入確認頁的事件（就緒、按下、token、取消、失敗）。只收事件、不回傳任何資料；沒有等待者就丟棄。
#[tauri::command]
pub fn tw_recaptcha_event(event: PageEvent, state: State<'_, RecaptchaState>) {
    deliver(&state.slot, event);
}

fn open_window(app: &AppHandle, otp_in_dialog: bool) -> AppResult<WebviewWindow> {
    let blank = parse_url(BLANK_URL)?;
    let height = if otp_in_dialog {
        WINDOW_HEIGHT_WITH_OTP
    } else {
        WINDOW_HEIGHT
    };
    WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::External(blank))
        .title(WINDOW_TITLE)
        .inner_size(WINDOW_WIDTH, height)
        .resizable(false)
        .maximizable(false)
        // 等使用者按鈕不設期限：不讓對話框縮到看不見的地方。
        .minimizable(false)
        .center()
        .visible(true)
        .focused(true)
        .on_navigation(is_allowed_navigation)
        .build()
        .map_err(|e| recaptcha_error(format!("無法開啟登入確認視窗：{e}")))
}

/// 內建頁填入帳號名稱（HTML escape）與 OTP 模式。先填 OTP 模式：帳號名稱裡的 `{{otp}}` 字樣不會被替換。
fn render_page(account: &str, otp_in_dialog: bool) -> String {
    PAGE_HTML
        .replace("{{otp}}", if otp_in_dialog { "manual" } else { "none" })
        .replace("{{account}}", &escape_html(account))
}

fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            c => escaped.push(c),
        }
    }
    escaped
}

/// 只管頂層文件（wry 掛在 NavigationStarting）；reCAPTCHA 的 iframe 不經過這裡。
fn is_allowed_navigation(url: &Url) -> bool {
    let url = url.as_str();
    url == BLANK_URL || url == PAGE_URL
}

/// 在 UI 執行緒上設定攔截，等到結果才返回；失敗時呼叫端不得導航。
async fn install_interceptor(window: &WebviewWindow, page: String) -> AppResult<()> {
    let (setup_tx, setup_rx) = oneshot::channel::<Result<(), String>>();
    window
        .with_webview(move |webview| {
            let installed = register_page_handler(&webview, page).map_err(|e| e.message());
            let _ = setup_tx.send(installed); // 失敗代表 acquire 已結束，沒有人需要結果
        })
        .map_err(|e| recaptcha_error(format!("無法設定頁面攔截：{e}")))?;

    match timeout(SETUP_TIMEOUT, setup_rx).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(message))) => Err(recaptcha_error(format!("無法設定頁面攔截：{message}"))),
        Ok(Err(_)) => Err(recaptcha_error("無法設定頁面攔截：視窗已關閉")),
        Err(_) => Err(recaptcha_error("無法設定頁面攔截：逾時")),
    }
}

/// `with_webview` closure 內呼叫（WebView2 的 UI 執行緒）。
fn register_page_handler(webview: &PlatformWebview, page: String) -> windows::core::Result<()> {
    let environment = webview.environment();
    // 先實際建一次回應：IStream 或 response 建不出來就整個設定失敗，不會導航。
    page_response(&environment, &page)?;

    let handler = WebResourceRequestedEventHandler::create(Box::new(move |_, args| match args {
        Some(args) => serve_page(&environment, &args, &page),
        None => Ok(()),
    }));
    let filter = HSTRING::from(PAGE_URL);
    let mut token = 0i64;
    // SAFETY: with_webview closure 內、UI 執行緒呼叫；controller 由 wry 持有且有效，filter／token 呼叫期間有效。quality-allow: WebView2 COM 綁定只有 unsafe fn
    unsafe {
        let core = webview.controller().CoreWebView2()?;
        core.AddWebResourceRequestedFilter(&filter, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL)?;
        core.add_WebResourceRequested(&handler, &mut token)?;
    }
    Ok(())
}

/// WebResourceRequested handler。這個 webview 所有 filter 的請求（含 Tauri 的 IPC）都會進來，只接手本頁。
fn serve_page(
    environment: &ICoreWebView2Environment,
    args: &ICoreWebView2WebResourceRequestedEventArgs,
    page: &str,
) -> windows::core::Result<()> {
    let mut uri = PWSTR::null();
    // SAFETY: handler 在 UI 執行緒同步執行，args 呼叫期間有效；Uri 字串交給 take_pwstr 以 CoTaskMemFree 釋放。quality-allow: WebView2 COM 綁定只有 unsafe fn
    unsafe {
        args.Request()?.Uri(&mut uri)?;
    }
    if take_pwstr(uri) != PAGE_URL {
        return Ok(());
    }
    let response = match page_response(environment, page) {
        Ok(response) => response,
        // 建不出內建頁也不能讓請求落到真站：改回空白 502。
        Err(_) => blocked_response(environment)?,
    };
    // SAFETY: handler 在 UI 執行緒同步執行，args 與 response 呼叫期間有效。quality-allow: WebView2 COM 綁定只有 unsafe fn
    unsafe { args.SetResponse(&response) }
}

fn page_response(
    environment: &ICoreWebView2Environment,
    page: &str,
) -> windows::core::Result<ICoreWebView2WebResourceResponse> {
    // SAFETY: SHCreateMemStream 複製輸入 bytes，IStream 由 COM 參考計數管理；environment 由 wry 持有且有效。quality-allow: Win32／WebView2 COM 綁定只有 unsafe fn
    unsafe {
        let stream = SHCreateMemStream(Some(page.as_bytes()))
            .ok_or_else(|| windows::core::Error::from_hresult(E_OUTOFMEMORY))?;
        environment.CreateWebResourceResponse(
            &stream,
            200,
            &HSTRING::from("OK"),
            &HSTRING::from(PAGE_HEADERS),
        )
    }
}

fn blocked_response(
    environment: &ICoreWebView2Environment,
) -> windows::core::Result<ICoreWebView2WebResourceResponse> {
    // SAFETY: environment 由 wry 持有且有效，參數在呼叫期間有效。quality-allow: WebView2 COM 綁定只有 unsafe fn
    unsafe {
        environment.CreateWebResourceResponse(
            None,
            502,
            &HSTRING::from("Bad Gateway"),
            &HSTRING::new(),
        )
    }
}

fn parse_url(url: &str) -> AppResult<Url> {
    url.parse::<Url>()
        .map_err(|e| recaptcha_error(format!("內建 URL 無效：{e}")))
}

fn recaptcha_error(context: impl Into<String>) -> AppError {
    AppError::Recaptcha {
        context: context.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: Timeouts = Timeouts {
        ready: Duration::from_millis(100),
        token: Duration::from_millis(100),
    };

    fn token_event(otp: Option<&str>) -> PageEvent {
        PageEvent::Token {
            token: "token".into(),
            otp: otp.map(str::to_string),
        }
    }

    /// 開一次對話框的交接點；回傳 slot、generation 與交給 `wait_for_token` 的接收端。
    fn begin_dialog() -> (Mutex<Slot>, u64, mpsc::UnboundedReceiver<PageEvent>) {
        let slot = Mutex::new(Slot::default());
        let (generation, events) = begin(&slot).expect("begin");
        (slot, generation, events)
    }

    #[tokio::test]
    async fn cancel_button_reports_cancelled() {
        let (slot, _, events) = begin_dialog();
        deliver(&slot, PageEvent::Ready);
        deliver(&slot, PageEvent::Cancel);
        let result = wait_for_token(events, false, &SHORT).await;
        assert!(matches!(result, Err(AppError::RecaptchaCancelled)));
    }

    /// 按下「登入」後、token 回來前關窗也是取消。
    #[tokio::test]
    async fn closing_window_reports_cancelled() {
        let (slot, generation, events) = begin_dialog();
        deliver(&slot, PageEvent::Ready);
        deliver(&slot, PageEvent::Submit);
        end(&slot, generation); // WindowEvent::Destroyed
        let result = wait_for_token(events, false, &SHORT).await;
        assert!(matches!(result, Err(AppError::RecaptchaCancelled)));
    }

    /// 交回 token 後轉成登入中：遲到的取消不影響這次登入，關窗被擋下。
    #[tokio::test]
    async fn token_hands_over_and_ignores_later_cancel() {
        let (slot, generation, events) = begin_dialog();
        deliver(&slot, PageEvent::Ready);
        deliver(&slot, PageEvent::Submit);
        deliver(&slot, token_event(None));
        deliver(&slot, PageEvent::Cancel);
        assert!(is_logging_in(&slot, generation));
        let granted = wait_for_token(events, false, &SHORT).await.expect("token");
        assert_eq!(granted.token, "token");
        assert!(granted.otp.is_none());
    }

    /// 就緒後等使用者按鈕不逾時；按下後才開始算 token 的期限。
    #[tokio::test]
    async fn waits_for_click_without_deadline_but_token_has_one() {
        let (slot, _, events) = begin_dialog();
        deliver(&slot, PageEvent::Ready);
        let waiting = tokio::spawn(async move { wait_for_token(events, false, &SHORT).await });
        tokio::time::sleep(SHORT.ready * 3).await;
        assert!(!waiting.is_finished(), "等按鈕不該逾時");
        deliver(&slot, PageEvent::Submit);
        let result = waiting.await.expect("join");
        assert!(matches!(result, Err(AppError::Recaptcha { .. })));
    }

    #[tokio::test]
    async fn manual_otp_comes_back_with_token() {
        let (slot, _, events) = begin_dialog();
        deliver(&slot, PageEvent::Submit);
        deliver(&slot, token_event(Some("012345")));
        let granted = wait_for_token(events, true, &SHORT).await.expect("token");
        assert_eq!(granted.otp.as_deref(), Some("012345"));
    }
}
