import { check, type Update } from "@tauri-apps/plugin-updater";
import { api } from "./api";
import { errorText, h, run, say, setConfig } from "./app";
import { installing } from "./dalamud";
import { gameUpdating, integrityCheck } from "./game-update";
import { launching } from "./login";
import { MB } from "./progress";

// ---------- 程式更新 ----------

/** 檢查更新的逾時；網路卡住時版本號旁的「檢查中…」不會一直掛著。 */
const UPDATE_CHECK_TIMEOUT_MS = 15_000;
/**
 * 背景檢查新版的間隔。程式常整天開著（遊戲執行期間也開著），只在啟動時查就一直看不到之後發的版；
 * 發版是幾天一次，一小時內看到就夠，一天最多 24 次 GET latest.json（release 靜態檔，不佔 GitHub API 額度）。
 */
const UPDATE_CHECK_INTERVAL_MS = 60 * 60 * 1000;
/**
 * 多久看一次「距上次檢查是否已超過間隔」：只比時間、不連網。用短週期輪詢而不是一小時一次的 setInterval，
 * 視窗取得焦點時補查不會跟計時器撞在一起重複查，睡眠喚醒後也很快補上。
 */
export const UPDATE_CHECK_TICK_MS = 5 * 60 * 1000;
/** 手動檢查的結果（已是最新版／檢查失敗）在版本號旁顯示多久。 */
const UPDATE_CHECK_RESULT_MS = 5000;
/**
 * 安裝版：install_app_update 啟動 NSIS 安裝程式後就結束本程式，重新開啟交給 NSIS 的 /R
 * （tauri.conf.json `plugins.updater.windows.installMode` = passive），所以沒有接 process plugin 的 relaunch。
 * 免安裝版：後端替換 exe、啟動新版後結束本程式。
 */
const UPDATE_INSTALLING_TEXT = "正在安裝，完成後會自動重新開啟";

/** 伺服器上的新版（JS check() 的結果，只用來顯示標題列的更新按鈕）；null＝沒有新版或還沒查到。持有後端資源，換掉時要 close()。 */
let appUpdate: Update | null = null;
/** 「更新至…」進行中：已下載／總位元組（伺服器沒給長度時為 null）；finished＝下載完並通過驗簽、交給安裝程式。 */
let updating: { done: number; total: number | null; finished: boolean } | null = null;
/** 上次更新失敗的原因：顯示在按鈕旁，按鈕改為「重試更新」；查到別的版本或重試時清掉。 */
let updateError: string | null = null;
/** 進行中的檢查：背景與手動共用同一次，不重複連網。 */
let updateCheck: Promise<Update | null> | null = null;
/**
 * check() 要帶的 updater target：免安裝版 `windows-x86_64-portable`，安裝版 undefined（plugin 預設 `windows-x86_64`）。
 * 執行中不會變，後端判定一次就沿用。
 */
let updateTarget: Promise<string | undefined> | null = null;
/** 上次開始檢查的時間（Date.now()）；0＝還沒查過。 */
let lastUpdateCheck = 0;
let versionStatusTimer: number | undefined;

export const versionButton = document.getElementById("app-version") as HTMLButtonElement;
const versionStatus = document.getElementById("app-version-status") as HTMLSpanElement;

interface AppUpdateView {
  root: HTMLElement;
  notes: HTMLDetailsElement;
  notesBody: HTMLElement;
  go: HTMLButtonElement;
  reason: HTMLElement;
  error: HTMLElement;
  progress: HTMLElement;
  progressText: HTMLElement;
  progressBar: HTMLProgressElement;
}

/** 標題列右端、版本號右邊的更新入口：只建一次，之後就地更新，重繪不會收起展開中的更新說明。沒有「稍後」：不擋畫面，也就不必關。 */
function appUpdateView(): AppUpdateView {
  const reason = h("span", { class: "app-update-note", id: "app-update-reason" });
  const error = h("span", { class: "app-update-note error", id: "app-update-error" });
  const progressText = h("span");
  const progressBar = h("progress", { "aria-label": "下載更新" });
  const progress = h("div", { class: "app-update-progress" }, progressText, progressBar);
  // 更新說明來自 latest.json（未簽章），一律當純文字顯示；浮在畫面上，不推擠標題列與主畫面。
  const summary = h("summary", {}, "更新說明");
  const notesBody = h("div", { class: "app-update-notes-body" });
  const notes = h("details", { class: "app-update-notes" }, summary, notesBody);
  notes.addEventListener("keydown", (ev) => {
    if (ev.key !== "Escape" || !notes.open) return;
    notes.open = false;
    summary.focus();
  });
  const go = h("button", { class: "primary app-update-go", type: "button" });
  go.addEventListener("click", startAppUpdate);
  // 更新說明緊接版本號；停用原因、失敗訊息緊貼按鈕左邊；下載中進度取代按鈕的位置。
  const root = h("div", { class: "app-update", role: "group", "aria-label": "程式更新" }, notes, reason, error, progress, go);
  root.hidden = true;
  return { root, notes, notesBody, go, reason, error, progress, progressText, progressBar };
}

const appUpdateEntry = appUpdateView();
versionButton.after(appUpdateEntry.root);

/**
 * 「更新至…」會結束本程式：登入、下載或套用、檢查進行中時不能更新，回傳原因。遊戲執行中可以更新：
 * 執行中帳號與 Dalamud 佔用存在 state 檔、ACT 切回啟動時重新等待，本程式重開後照常接手，遊戲不受影響。
 */
function updateBlockReason(): string | null {
  if (launching) return "正在登入，完成後才能更新";
  if (installing) return "正在下載 Dalamud，完成後才能更新";
  if (gameUpdating) return "正在更新遊戲，完成後才能更新";
  if (integrityCheck) return "正在檢查遊戲檔，完成後才能更新";
  return null;
}

export function paintAppUpdate(): void {
  const v = appUpdateEntry;
  v.root.hidden = appUpdate === null;
  if (!appUpdate) return;
  v.go.hidden = updating !== null;
  v.progress.hidden = updating === null;
  if (updating) {
    v.reason.hidden = true;
    v.error.hidden = true;
    const { done, total, finished } = updating;
    if (finished) {
      v.progressText.textContent = UPDATE_INSTALLING_TEXT;
      v.progressBar.removeAttribute("value");
    } else if (total) {
      v.progressText.textContent = `下載更新 ${Math.floor((done * 100) / total)}%（${(done / MB).toFixed(1)} / ${(total / MB).toFixed(1)} MB）`;
      v.progressBar.max = total;
      v.progressBar.value = done;
    } else {
      v.progressText.textContent = `下載更新 ${(done / MB).toFixed(1)} MB`;
      v.progressBar.removeAttribute("value");
    }
    return;
  }
  const reason = updateBlockReason();
  v.go.disabled = reason !== null;
  v.go.textContent = updateError ? "重試更新" : `更新至 ${appUpdate.version}`;
  v.reason.textContent = reason ?? "";
  v.reason.hidden = reason === null;
  v.error.textContent = updateError ?? "";
  // 錯誤訊息可能很長：標題列只顯示一行，完整內容在滑鼠停留說明與下方訊息列。
  v.error.title = updateError ?? "";
  v.error.hidden = updateError === null;
  // 隱藏的元素仍會被 aria-describedby 念出，只關聯看得到的。
  const describedBy = [reason === null ? "" : v.reason.id, updateError === null ? "" : v.error.id].filter(Boolean).join(" ");
  if (describedBy) v.go.setAttribute("aria-describedby", describedBy);
  else v.go.removeAttribute("aria-describedby");
}

function releaseUpdate(u: Update): void {
  u.close().catch((e: unknown) => console.warn("釋放更新資源失敗", e));
}

/** 換上剛查到的結果（null＝沒有新版）並重畫更新入口；舊的 Update 釋放後端資源。下載中不換掉正在用的。 */
function setAppUpdate(next: Update | null): void {
  if (updating) {
    if (next) releaseUpdate(next);
    return;
  }
  const prev = appUpdate;
  if (prev) releaseUpdate(prev);
  appUpdate = next;
  // 背景定期重查到同一版時不收起正在看的更新說明，也不清掉上次失敗的訊息。
  if (next?.version !== prev?.version) {
    updateError = null;
    appUpdateEntry.notes.open = false;
  }
  if (next) {
    const notes = next.body?.trim() ?? "";
    appUpdateEntry.notesBody.textContent = notes;
    appUpdateEntry.notes.hidden = notes === "";
  }
  paintAppUpdate();
}

function startAppUpdate(): void {
  if (!appUpdate) return;
  void run("更新程式", async () => {
    // 按鈕狀態停在上次重繪：遊戲可能已經關了或又開了，以現在的狀態為準。
    setConfig(await api.getConfig());
    const reason = updateBlockReason();
    if (reason) throw new Error(`${reason}。`);
    const progress = { done: 0, total: null as number | null, finished: false };
    updating = progress;
    updateError = null;
    appUpdateEntry.notes.open = false;
    paintAppUpdate();
    try {
      // 後端重新查一次最新版，分段下載、驗簽後才安裝（plugin 的 downloadAndInstall 只開一條連線）。
      await api.installAppUpdate((p) => {
        if (p.kind === "download") {
          progress.done = p.done;
          progress.total = p.total;
        } else {
          progress.finished = true;
        }
        paintAppUpdate();
      });
    } catch (e) {
      updating = null;
      updateError = `更新失敗：${errorText(e)}`;
      throw new Error(updateError);
    }
    // 安裝版到不了這裡（安裝程式一啟動本程式就結束）；免安裝版在新版開始執行後回到這裡，本程式隨即結束。
    return UPDATE_INSTALLING_TEXT;
  });
}

/** 查一次新版並套用結果，回傳目前的新版（沒有＝null）。背景與手動共用進行中的那一次；更新進行中不查。 */
function checkForUpdate(): Promise<Update | null> {
  if (updating) return Promise.resolve(appUpdate);
  if (!updateCheck) {
    lastUpdateCheck = Date.now();
    updateTarget ??= api.appUpdateTarget().then((target) => target ?? undefined);
    updateCheck = updateTarget
      .then((target) => check({ timeout: UPDATE_CHECK_TIMEOUT_MS, target }))
      .then((next) => {
        setAppUpdate(next);
        return appUpdate;
      })
      .finally(() => (updateCheck = null));
  }
  return updateCheck;
}

/** 距上次檢查超過間隔才查。查不到（開發模式、沒網路、還沒發布）只記在 console，不打擾使用者也不擋主程式。 */
export function backgroundUpdateCheck(): void {
  if (Date.now() - lastUpdateCheck < UPDATE_CHECK_INTERVAL_MS) return;
  checkForUpdate().catch((e: unknown) => console.warn("背景檢查更新失敗", e));
}

/** 版本號旁的檢查結果；transient＝幾秒後自動清掉。 */
function showVersionStatus(text: string, kind: "ok" | "error" | "", transient: boolean): void {
  window.clearTimeout(versionStatusTimer);
  versionStatus.textContent = text;
  versionStatus.className = kind ? `app-version-status ${kind}` : "app-version-status";
  if (transient) versionStatusTimer = window.setTimeout(() => (versionStatus.textContent = ""), UPDATE_CHECK_RESULT_MS);
}

/** 按版本號：立即檢查。不走 run()（只是查詢，不鎖其他按鈕、不重繪）；有新版時更新按鈕會出現在版本號右邊。 */
async function manualUpdateCheck(): Promise<void> {
  if (updating) return;
  showVersionStatus("檢查中…", "", false);
  try {
    const next = await checkForUpdate();
    showVersionStatus(next ? `有新版本 ${next.version}` : "已是最新版", "ok", true);
  } catch (e) {
    showVersionStatus("檢查失敗", "error", true);
    say(`檢查更新失敗：${errorText(e)}`, "error");
  }
}

versionButton.addEventListener("click", () => void manualUpdateCheck());
