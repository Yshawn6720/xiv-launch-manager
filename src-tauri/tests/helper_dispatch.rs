//! 提權 helper 入口（主 note「信任邊界（方案 1）」）：實際的 exe 帶 `--apply-patches` 時，在 `run()` 與
//! single-instance plugin 之前分派成 helper，以 helper 的 exit code 結束，不建 Tauri 視窗。
//! 分派若壞掉，exe 會長成 GUI（逾時），或在主程式存活時把命令列交給第一實例後 exit 0，都不會是 2 或 10。
//! 一般權限、不經 UAC：參數不符或 job 讀不到，走不到 HKLM 解析與寫入。

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const EXE: &str = env!("CARGO_BIN_EXE_xiv-launch-manager");
const SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const DEADLINE: Duration = Duration::from_secs(30);

/// 執行 exe 並等它自行結束；逾時即結束它並失敗。
fn exit_code(args: &[&str]) -> i32 {
    let mut child = Command::new(EXE)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn exe");
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status.code().expect("exit code");
        }
        if start.elapsed() > DEADLINE {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{args:?} 未在 {DEADLINE:?} 內結束：沒有分派成 helper");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn malformed_helper_invocation_exits_2_instead_of_starting_gui() {
    assert_eq!(exit_code(&["--apply-patches"]), 2);
    assert_eq!(exit_code(&["--apply-patches", "job.json", SHA]), 2, "job 不是絕對路徑");
    assert_eq!(exit_code(&["--minimized", "--apply-patches", r"C:\job.json", SHA]), 2);
}

#[test]
fn helper_with_unreadable_job_exits_10_without_creating_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let job = dir.path().join("job.json");
    let job = job.to_str().expect("utf8 path");
    assert_eq!(exit_code(&["--apply-patches", job, SHA]), 10);
    assert_eq!(std::fs::read_dir(dir.path()).expect("read_dir").count(), 0);
}
