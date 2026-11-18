// 執行：node --test "test/*.test.mjs"（Node 22.18+／23.6+ 直接載入 .ts）
import assert from "node:assert/strict";
import { test } from "node:test";
import { canContinueAfterDalamudPrepareError, clearEarlyExits, rememberEarlyExit, syncRunning, takeEarlyExit } from "../src/running-sync.ts";

const versionMismatch = "Dalamud 目前只支援遊戲版本 2026.11.04.0000.0000，本機遊戲已是 2026.09.14.0000.0000";
for (const [title, region, message, continues] of [
  ["國際服版本不符仍登入以檢查遊戲更新", "global", versionMismatch, true],
  ["繁中服不因版本不符續跑", "tw", versionMismatch, false],
  ["國際服其他衛月錯誤不續跑", "global", "安裝失敗（下載 Dalamud）", false],
  ["國際服取消準備不續跑", "global", "已取消", false],
]) {
  test(title, () => assert.equal(canContinueAfterDalamudPrepareError(region, message), continues));
}

const main = { accountId: "tw-main", pid: 67244 };
const intl = { accountId: "global-1", pid: 51000 };
const notice = { accountId: "tw-main", pid: 67244, label: "主號", text: "已啟動 主號（pid 67244）・Dalamud 13.0.0.16" };

test("提早結束只交給同一個 pid 的登入結果，取出一次，新登入不沿用舊事件", () => {
  const message = "遊戲啟動後立即結束";
  rememberEarlyExit(main.pid, message);
  assert.equal(takeEarlyExit(intl.pid), undefined);
  assert.equal(takeEarlyExit(main.pid), message);
  assert.equal(takeEarlyExit(main.pid), undefined);
  rememberEarlyExit(main.pid, message);
  clearEarlyExits();
  assert.equal(takeEarlyExit(main.pid), undefined);
});

test("兩個遊戲都關了：側欄執行中標記清空、訊息列的啟動結果改成已結束", () => {
  const r = syncRunning(notice, notice.text, [], []);
  assert.deepEqual(r.accountsRunning, []);
  assert.equal(r.message, "主號 的遊戲已結束（pid 67244）");
});

test("只關了一個：只撤那個帳號", () => {
  const r = syncRunning(notice, notice.text, [main], []);
  assert.deepEqual(r.accountsRunning, ["tw-main"]);
  assert.equal(r.message, null);
});

test("兩個都還在：照列、訊息不動", () => {
  const r = syncRunning(notice, notice.text, [intl, main], []);
  assert.deepEqual(r.accountsRunning, ["global-1", "tw-main"]);
  assert.equal(r.message, null);
});

test("同帳號重開（pid 換了）：舊的啟動訊息改成已結束，帳號仍標執行中", () => {
  const r = syncRunning(notice, notice.text, [{ accountId: "tw-main", pid: 70000 }], []);
  assert.deepEqual(r.accountsRunning, ["tw-main"]);
  assert.equal(r.message, "主號 的遊戲已結束（pid 67244）");
});

test("訊息列已換成別的內容：不覆蓋", () => {
  const r = syncRunning(notice, "登入 國際…", [], []);
  assert.deepEqual(r.accountsRunning, []);
  assert.equal(r.message, null);
});

test("用設定檔的遊戲關了：Dalamud 分頁的「使用中」撤掉、仍在用的照列", () => {
  assert.deepEqual(syncRunning(null, "", [], []).profilesInUse, []);
  assert.deepEqual(syncRunning(null, "", [main], ["p-shared"]).profilesInUse, ["p-shared"]);
});
