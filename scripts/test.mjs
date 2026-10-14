#!/usr/bin/env node
// 跑 Rust 測試，等同 `cargo test --manifest-path src-tauri/Cargo.toml [-- <args>]`，但測試 exe 從固定路徑執行：
//   node scripts/test.mjs [<libtest 參數…>]   例：node scripts/test.mjs dalamud::tests
// 為什麼：開發機的 AdGuard 以 exe 路徑決定是否過濾（連 loopback 也過濾），被過濾的測試 exe 跑
// dalamud::install 的本機 stub 下載測試會 os error 10054；cargo 的測試 exe 檔名帶 hash，hash 隨版本號改變，
// 每次發版都是新路徑、要重新排除。改成先 `--no-run` 編譯，把測試 exe 複製到
// src-tauri/target/stable-test/<target 名>.exe 再執行，AdGuard 只需排除一次。
// 與 cargo test 的等價性：cwd＝package 根（src-tauri）；環境沿用本行程。cargo 執行測試時另注入的
// CARGO_MANIFEST_DIR／CARGO_PKG_* 等程式碼只在編譯期（env!）讀、runtime 沒讀，故不補；PATH 的 dylib
// 搜尋路徑也不補（測試 exe 只匯入系統 DLL，移到別的目錄不影響載入）。doc-test 仍交給 `cargo test --doc`
// （rustdoc 把 doc-test 編到暫存目錄，無法固定路徑）。與 cargo 相同，某個測試 exe 失敗就停、以其結束碼結束。
// 只用 Node 內建模組；子程序一律 argv 陣列、不經 shell。
import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const MANIFEST = join(ROOT, "src-tauri", "Cargo.toml");
const STABLE_DIR = join(ROOT, "src-tauri", "target", "stable-test");

/**
 * @param {string} message
 * @returns {never}
 */
function fail(message) {
  console.error(`test: ${message}`);
  process.exit(1);
}

/**
 * 子程序結束碼；被訊號終止或無法啟動時給 1。
 * @param {import("node:child_process").SpawnSyncReturns<unknown>} result
 * @param {string} what
 */
function exitCode(result, what) {
  if (result.error) fail(`${what} 無法執行：${result.error.message}`);
  return result.status ?? 1;
}

/**
 * 編譯測試但不執行，回傳 cargo 回報的測試 exe（依 cargo 輸出順序）。
 * @returns {{ name: string; src: string; executable: string; packageDir: string }[]}
 */
function buildTests() {
  const result = spawnSync(
    "cargo",
    ["test", "--manifest-path", MANIFEST, "--no-run", "--message-format=json-render-diagnostics"],
    { cwd: ROOT, stdio: ["inherit", "pipe", "inherit"], encoding: "utf8", maxBuffer: 256 * 1024 * 1024 },
  );
  const code = exitCode(result, "cargo test --no-run");
  if (code !== 0) process.exit(code);
  const tests = [];
  for (const line of result.stdout.split(/\r?\n/)) {
    if (!line.startsWith("{")) continue;
    const msg = JSON.parse(line);
    if (msg.reason !== "compiler-artifact" || msg.profile?.test !== true || !msg.executable) continue;
    tests.push({
      name: msg.target.name.replaceAll("-", "_"),
      src: msg.target.src_path,
      executable: msg.executable,
      packageDir: dirname(msg.manifest_path),
    });
  }
  if (tests.length === 0) fail("cargo 沒有回報任何測試 exe");
  return tests;
}

/**
 * 複製到固定路徑；同名覆寫，目標被鎖（例如上一輪測試還在跑）時 copyFileSync 拋錯即失敗。
 * @param {{ name: string; executable: string }[]} tests
 * @returns {string[]} 各測試的固定路徑
 */
function stage(tests) {
  mkdirSync(STABLE_DIR, { recursive: true });
  const seen = new Set();
  return tests.map(({ name, executable }) => {
    if (seen.has(name)) fail(`多個測試 target 同名 ${name}，固定路徑會互相覆蓋`);
    seen.add(name);
    const dest = join(STABLE_DIR, `${name}.exe`);
    try {
      copyFileSync(executable, dest);
    } catch (e) {
      fail(`無法複製 ${executable} → ${dest}：${e instanceof Error ? e.message : String(e)}`);
    }
    return dest;
  });
}

const args = process.argv.slice(2);
const tests = buildTests();
const staged = stage(tests);
for (const [i, { src, packageDir }] of tests.entries()) {
  console.error(`     Running ${relative(packageDir, src)} (${relative(ROOT, staged[i])})`);
  const code = exitCode(spawnSync(staged[i], args, { cwd: packageDir, stdio: "inherit" }), staged[i]);
  if (code !== 0) process.exit(code);
}
const doc = spawnSync("cargo", ["test", "--manifest-path", MANIFEST, "--doc", "--", ...args], { cwd: ROOT, stdio: "inherit" });
process.exit(exitCode(doc, "cargo test --doc"));
