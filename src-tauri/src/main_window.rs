//! 主視窗與設定分頁「介面」：視窗主題（標題列與 WebView2 的 `prefers-color-scheme`），以及本程式成功啟動遊戲後
//! 收起主視窗（最小化／縮到系統匣）、本程式啟動的遊戲全部結束後還原。
//!
//! 「本程式啟動的遊戲」沿用 `state/accounts_running.json`（header「執行中帳號」同一份紀錄）；等遊戲結束沿用
//! [`dalamud::wait_for_exit`]（`WaitForSingleObject`，與 ACT 連動的切回相同，不輪詢）。

use std::collections::{BTreeMap, HashSet};
use std::fmt::Display;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewWindow, WindowEvent};

use crate::dalamud::{self, GameProcess};
use crate::error::AppResult;
use crate::store::{self, AfterLaunch, AppPaths, Theme};

/// tauri.conf.json 的主視窗 label。
const MAIN_LABEL: &str = "main";
const TRAY_ID: &str = "main";
const MENU_SHOW: &str = "tray-show";
const MENU_QUIT: &str = "tray-quit";

/// 套到 Tauri 視窗的主題；跟隨系統為 `None`。
pub fn window_theme(theme: Theme) -> Option<tauri::Theme> {
    match theme {
        Theme::System => None,
        Theme::Dark => Some(tauri::Theme::Dark),
        Theme::Light => Some(tauri::Theme::Light),
    }
}

/// 遊戲啟動後收起、全部結束後還原。clone 共用同一份狀態（等待執行緒各持一份）。
///
/// 鎖內只讀寫狀態與檔案，不呼叫視窗或系統匣的 API：那些要到主執行緒執行，而主執行緒的視窗事件（取得焦點）
/// 也會取這把鎖。
#[derive(Clone)]
pub struct MainWindow(Arc<Shared>);

struct Shared {
    paths: AppPaths,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// 本程式依設定收起了視窗、還沒還原；使用者自己叫回視窗（視窗取得焦點）也算還原。
    collapsed: bool,
    /// 正在等待結束的遊戲；同一個程序只開一條等待執行緒。
    watching: HashSet<GameProcess>,
}

impl MainWindow {
    pub fn new(paths: AppPaths) -> Self {
        Self(Arc::new(Shared {
            paths,
            state: Mutex::new(State::default()),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // 臨界區只改旗標與 set，panic 不會留下半更新的狀態，poison 後可直接沿用。
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 程式啟動時（setup）：套用已存的主題；主視窗取得焦點（使用者自己叫回視窗）時結束這次收起。
    pub fn setup(&self, app: &AppHandle) {
        match store::load_config(&self.0.paths) {
            Ok(config) => {
                if let Some(theme) = window_theme(config.ui.theme) {
                    app.set_theme(Some(theme));
                }
            }
            // 前端讀設定時會在訊息列顯示同一個錯誤。
            Err(e) => log_error("讀不到設定，視窗主題先跟隨系統", &e),
        }
        let Some(window) = app.get_webview_window(MAIN_LABEL) else {
            return;
        };
        let this = self.clone();
        let handle = app.clone();
        window.on_window_event(move |event| {
            if let WindowEvent::Focused(true) = event {
                this.returned(&handle);
            }
        });
    }

    /// 本程式成功啟動遊戲之後（`launch_account` 回 Ok：遊戲程序已開出、登入後自動開 ACT 的 UAC 也已回應）：依
    /// 「介面 › 遊戲啟動後」收起主視窗，並等本程式啟動、仍在執行的每個遊戲結束，全部結束時還原。沒有仍在執行的
    /// 遊戲（例如紀錄沒寫成、或已提早結束）就不收起：沒有能等的遊戲，就沒有還原的時機。錯誤只記 log：遊戲已經開起來，
    /// 不能回報成啟動失敗。之後背景觀察到這個遊戲提早結束時，呼叫端另以 [`Self::restore`] 叫回視窗顯示錯誤。
    pub fn after_game_started(&self, app: &AppHandle) {
        let after_launch = match store::load_config(&self.0.paths) {
            Ok(config) => config.ui.after_launch,
            Err(e) => return log_error("讀不到設定，視窗不收起", &e),
        };
        let collapse: fn(&AppHandle, &MainWindow) -> tauri::Result<()> = match after_launch {
            AfterLaunch::Keep => return,
            AfterLaunch::Minimize => |app, _| main_window(app)?.minimize(),
            AfterLaunch::Tray => |app, this| {
                // 先放系統匣圖示再藏視窗：圖示放不上去時視窗不能藏起來，否則叫不回來。
                show_tray(app, this)?;
                main_window(app)?.hide()
            },
        };
        let games = match running_games(&self.0.paths) {
            Ok(games) if games.is_empty() => return,
            Ok(games) => games,
            Err(e) => return log_error("讀不到執行中的遊戲，視窗不收起", &e),
        };
        if let Err(e) = collapse(app, self) {
            hide_tray(app);
            return log_error("無法收起視窗", &e);
        }
        // 收起之後才開始等：等待中的遊戲在這之前就結束的，等待立刻返回，看得到「已收起」而還原。
        let mut state = self.lock();
        state.collapsed = true;
        for game in games {
            self.watch(&mut state, app, game);
        }
    }

    /// 叫回主視窗並拿掉系統匣圖示：系統匣左鍵、選單「顯示視窗」、本程式啟動的遊戲全部結束、再次開啟本程式、
    /// 遊戲啟動後提早結束（顯示錯誤）。
    pub fn restore(&self, app: &AppHandle) {
        self.lock().collapsed = false;
        hide_tray(app);
        if let Err(e) = show_main(app) {
            log_error("無法還原視窗", &e);
        }
    }

    /// 主視窗取得焦點：使用者自己叫回了視窗（例如點工作列），這次收起結束；之後遊戲全部結束時不再搶到最上層。
    fn returned(&self, app: &AppHandle) {
        let was_collapsed = std::mem::take(&mut self.lock().collapsed);
        if was_collapsed {
            hide_tray(app);
        }
    }

    /// 開一條執行緒等 `game` 結束；已在等的程序不重複開。須持有鎖（`state`）。
    fn watch(&self, state: &mut State, app: &AppHandle, game: GameProcess) {
        if !state.watching.insert(game) {
            return;
        }
        let this = self.clone();
        let handle = app.clone();
        let spawned = thread::Builder::new().name("game-exit-watch".into()).spawn(move || {
            match dalamud::wait_for_exit(&game) {
                Ok(()) => this.game_ended(&handle, &game),
                Err(e) => {
                    log_error("無法等待遊戲結束，遊戲關掉時視窗不會自動還原", &e);
                    this.lock().watching.remove(&game);
                }
            }
        });
        if let Err(e) = spawned {
            state.watching.remove(&game);
            log_error("無法等待遊戲結束，遊戲關掉時視窗不會自動還原", &e);
        }
    }

    /// `game` 結束後：仍在收起中、且本程式啟動的遊戲已全部結束就還原。
    fn game_ended(&self, app: &AppHandle, game: &GameProcess) {
        let restore = {
            let mut state = self.lock();
            state.watching.remove(game);
            state.collapsed
                && match dalamud::account_processes(&self.0.paths) {
                    Ok(records) => all_ended(&records, dalamud::is_alive),
                    Err(e) => {
                        log_error("讀不到執行中的遊戲，視窗不自動還原", &e);
                        false
                    }
                }
        };
        if restore {
            self.restore(app);
        }
    }
}

/// 帳號 id → 遊戲程序的紀錄（`state/accounts_running.json`，可含已結束的）中仍在執行的遊戲；`alive` 判斷 pid
/// 與建立時間都相符。
fn alive_games(records: &BTreeMap<String, GameProcess>, alive: impl Fn(&GameProcess) -> bool) -> Vec<GameProcess> {
    records.values().copied().filter(|p| alive(p)).collect()
}

/// 本程式啟動的遊戲是否已全部結束（紀錄裡沒有仍在執行的）。
fn all_ended(records: &BTreeMap<String, GameProcess>, alive: impl Fn(&GameProcess) -> bool) -> bool {
    alive_games(records, alive).is_empty()
}

fn running_games(paths: &AppPaths) -> AppResult<Vec<GameProcess>> {
    Ok(alive_games(&dalamud::account_processes(paths)?, dalamud::is_alive))
}

fn main_window(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    app.get_webview_window(MAIN_LABEL).ok_or(tauri::Error::WindowNotFound)
}

fn show_main(app: &AppHandle) -> tauri::Result<()> {
    let window = main_window(app)?;
    window.show()?;
    window.unminimize()?;
    // 使用者正在用別的程式時，Windows 不讓背景程式搶前景，`set_focus` 拿不到焦點，視窗會還原在別的視窗後面；
    // 先切一次「最上層」再切回，把視窗排到一般視窗的最上面（調整前後順序不需要前景權限）。
    window.set_always_on_top(true)?;
    window.set_always_on_top(false)?;
    window.set_focus()
}

/// 顯示系統匣圖示（app 的既有 icon）。第一次收起時建立，之後只切換顯示：左鍵單擊還原視窗；右鍵選單「顯示視窗」
/// 「結束」。
fn show_tray(app: &AppHandle, this: &MainWindow) -> tauri::Result<()> {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        return tray.set_visible(true);
    }
    let menu = Menu::with_items(
        app,
        &[
            &MenuItem::with_id(app, MENU_SHOW, "顯示視窗", true, None::<&str>)?,
            &MenuItem::with_id(app, MENU_QUIT, "結束", true, None::<&str>)?,
        ],
    )?;
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip(&app.package_info().name)
        .menu(&menu)
        .show_menu_on_left_click(false);
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    let on_menu = this.clone();
    let on_click = this.clone();
    builder
        .on_menu_event(move |app, event| match event.id().as_ref() {
            MENU_SHOW => on_menu.restore(app),
            MENU_QUIT => quit(app),
            _ => {}
        })
        .on_tray_icon_event(move |tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                on_click.restore(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn hide_tray(app: &AppHandle) {
    if let Some(tray) = app.tray_by_id(TRAY_ID)
        && let Err(e) = tray.set_visible(false)
    {
        log_error("無法拿掉系統匣圖示", &e);
    }
}

/// 系統匣選單「結束」：與按視窗的 X 相同，關閉主視窗，最後一個視窗關閉時 Tauri 結束程式。
fn quit(app: &AppHandle) {
    if let Err(e) = main_window(app).and_then(|window| window.close()) {
        log_error("無法關閉視窗", &e);
    }
}

/// 背景的收起與還原沒有畫面可回報，只記 log（stderr）。
fn log_error(what: &str, e: &dyn Display) {
    eprintln!("主視窗：{what}：{e}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(pid: u32) -> GameProcess {
        GameProcess { pid, created: u64::from(pid) * 10 }
    }

    #[test]
    fn all_ended_only_when_no_recorded_game_is_alive() {
        let records = BTreeMap::from([("a".to_string(), game(1)), ("b".to_string(), game(2))]);
        assert_eq!(alive_games(&records, |p| *p == game(2)), [game(2)]);
        assert!(!all_ended(&records, |p| *p == game(2)), "還有一個在執行");
        assert!(all_ended(&records, |_| false));
        assert!(all_ended(&BTreeMap::new(), |_| true), "沒有紀錄＝全部結束");
    }
}
