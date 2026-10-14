// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // 提權 helper（`--apply-patches`）必須在 run() 與 single-instance plugin 之前分派，不建 Tauri。
    if let Some(code) = xiv_launch_manager_lib::helper_dispatch() {
        std::process::exit(code)
    }
    // 免安裝版更新啟動的新程序：先等舊程序結束，再進入 run() 的單一實例判定。
    xiv_launch_manager_lib::finish_update_handoff();
    xiv_launch_manager_lib::run()
}
