mod act;
mod act_bridge;
mod act_plugin;
mod characters;
mod commands;
mod dalamud;
mod error;
mod folder_picker;
mod game;
mod global;
mod launch;
mod main_window;
mod news;
mod patch;
mod region;
mod secrets;
mod store;
mod totp;
mod tw;
mod update;

use tauri::Manager;
use tokio::sync::Mutex;

/// 提權 helper 入口：命令列是 `--apply-patches` 時執行 helper 並回傳 exit code，否則 None。
/// main.rs 在 [`run`]（與 single-instance plugin）之前呼叫，helper 不建 Tauri。
pub fn helper_dispatch() -> Option<i32> {
    patch::helper::dispatch_from_args()
}

/// 免安裝版更新的交接：由更新啟動的新程序先等舊程序結束（`update::finish_handoff`）。main.rs 在 [`run`]（單一實例
/// plugin）之前呼叫，否則舊程序還沒結束時，新程序會被當成第二個實例而退出。
pub fn finish_update_handoff() {
    update::finish_handoff()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // 單一實例：所有安裝與啟動共用同一把 app-wide mutex。
        // 縮到系統匣時主視窗是隱藏的：再次開啟本程式時叫回視窗並拿掉系統匣圖示。
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(main_window) = app.try_state::<main_window::MainWindow>() {
                main_window.restore(app);
            }
        }))
        // 自動更新：endpoint、pubkey、Windows installMode 在 tauri.conf.json plugins.updater。前端只用它的
        // check（免安裝版帶 target `windows-x86_64-portable`，見 commands::app_update_target）；下載、驗簽、安裝走
        // commands::install_app_update（分段下載；安裝版交給 NSIS、免安裝版原地替換 exe，見 update.rs）。
        // 不裝 process plugin：Windows 上 install 啟動 NSIS 後就結束本程式，重啟交給 NSIS 的 /R（passive 模式）；
        // 免安裝版由 update.rs 自己啟動新版。
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let paths = store::AppPaths::from_env()?;
            // 免安裝版上次更新留下的 <exe>.old／.new：setup 只在單一實例判定通過後才跑，不會刪到第一個實例正在下載的檔。
            update::remove_leftovers();
            // ACT 連動外掛檔（protocols §6.3）：啟動時對齊嵌入版，ACT 下次啟動就用新版。
            // quality-allow: 失敗不影響本程式其他功能；「啟用 ACT 連動」「建立另一服的 ACT」執行前會再寫一次並回報錯誤
            let _ = act_bridge::Env::from_env().and_then(|env| act_bridge::ensure_plugin_file(&env.home));
            // ACT 連動（protocols §6.2）：對指令檔裡的目標重新開始等待，本程式關閉期間已結束的立刻切回。
            let act_targets = act_bridge::ActTargets::new(paths.clone());
            act_targets.resume();
            let character_usage = characters::usage::UsageWatch::new(paths.clone());
            // 設定 › 介面：套用已存的視窗主題；遊戲啟動後收起、全部結束後還原。
            let main_window = main_window::MainWindow::new(paths.clone());
            main_window.setup(app.handle());
            app.manage(main_window);
            app.manage(commands::AppState {
                paths,
                launch_lock: Mutex::new(()),
                config_lock: Mutex::new(()),
                download_cancel: dalamud::DownloadCancel::default(),
                pending_updates: patch::update::PendingUpdates::default(),
                act_targets,
                prepared_dalamud: dalamud::PreparedCache::default(),
                act_plugin_latest: act_plugin::LatestCache::default(),
                character_usage,
            });
            app.manage(tw::RecaptchaState::default());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::set_game_path,
            commands::set_theme,
            commands::set_after_launch,
            commands::set_external_pack,
            commands::detect_game_path,
            commands::detect_dalamud_data,
            commands::get_server_status,
            commands::open_folder,
            commands::pick_folder,
            commands::get_tw_news,
            commands::get_global_news,
            commands::open_news,
            commands::open_shop,
            commands::save_account,
            commands::set_account_profile,
            commands::import_detected_profile,
            commands::delete_account,
            commands::save_profile,
            commands::delete_profile,
            commands::duplicate_profile,
            commands::list_yanmucorp_releases,
            commands::list_installed_dalamud,
            commands::remove_installed_dalamud,
            commands::install_dalamud,
            commands::cancel_dalamud_download,
            commands::app_update_target,
            commands::install_app_update,
            commands::get_license_text,
            commands::launch_account,
            commands::preview_otp,
            commands::check_game_update,
            commands::update_game,
            commands::clear_patch_dirty,
            commands::get_patch_state,
            commands::check_game_integrity,
            commands::get_act_settings,
            commands::save_act_settings,
            commands::list_act_choices,
            commands::get_running_status,
            characters::commands::list_characters,
            characters::commands::set_character_label,
            characters::commands::backup_character,
            characters::commands::restore_character,
            characters::commands::plan_character_transfer,
            characters::commands::transfer_character,
            commands::launch_act,
            commands::get_act_link_status,
            commands::get_act_plugin_versions,
            commands::enable_act_link,
            commands::create_dual_act,
            tw::recaptcha::tw_recaptcha_event,
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|e| panic!("Tauri 啟動失敗：{e}")); // quality-allow: app 進入點無法回傳錯誤，啟動失敗只能終止
}
