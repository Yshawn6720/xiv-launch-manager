import { api, regionLabel, type ActStatus, type ActStatusByRegion, type RunningGame } from "./api";
import { paintActStatus } from "./act";
import { accountLabel, config, errorText, h, REGIONS, say, shownMessage } from "./app";
import { paintProfilesInUse } from "./dalamud";
import { paintAccountsRunning } from "./login";
import { type LaunchNotice, syncRunning } from "./running-sync";

// ---------- 執行狀態：header 狀態列；ACT 子分頁共用同一份結果 ----------

/** 視窗可見時多久查一次；每次會列舉一次系統程序（ACT 判定）。 */
const RUNNING_STATUS_POLL_MS = 5000;
const runningStatusBar = document.getElementById("running-status") as HTMLElement;
/** 最近一次查到的各區服 ACT 狀態；null＝還沒查到。查詢失敗沿用上次的結果。 */
export let actStatus: ActStatusByRegion | null = null;
/** 最近一次查到、本程式啟動且仍在執行的遊戲。 */
let runningGames: RunningGame[] = [];
let runningStatusSeq = 0;
/** 輪詢計時器；undefined＝停止（視窗隱藏時）。 */
let runningStatusTimer: number | undefined;
/** header 目前畫出的內容；沒變就不重建，免得 role=status 每次輪詢都重念。 */
let runningStatusShown = "";
/** 最近一次登入成功時訊息列的啟動結果；該遊戲結束後輪詢會改寫訊息列。 */
let launchNotice: LaunchNotice | null = null;

/** 登入成功、即將在訊息列顯示啟動結果時記下。登入前發出、晚到的輪詢結果看不到新遊戲，作廢以免誤判成已結束。 */
export function noteLaunch(notice: LaunchNotice): void {
  launchNotice = notice;
  runningStatusSeq++;
}

/** header 第一行的 ACT 狀態文字；null＝尚未設定 ACT，只寫區服名稱。 */
const HEADER_ACT: Record<ActStatus, { text: string; cls: string } | null> = {
  notConfigured: null,
  missing: { text: "找不到執行檔", cls: "warn" },
  running: { text: "執行中", cls: "status-dot" },
  stopped: { text: "未執行", cls: "muted" },
};

/**
 * header 狀態列：每個區服一組，第一行 ACT 狀態（未設定 ACT 時只寫區服名稱），第二行該區服執行中帳號的 PID
 * （ACT 選遊戲程序時要用）。兩服用同一個 ACT（單開）時仍分兩組、各寫同一個 ACT 的狀態；沒有 ACT 也沒有遊戲的區服不列。
 */
export function paintRunningStatus(): void {
  // 第一次 render() 讀到設定前 config 尚未指派：結果先存著，render() 會再畫。
  if (config === undefined) return;
  const pids = new Map(runningGames.map((g) => [g.accountId, g.pid]));
  const groups = REGIONS.flatMap((region) => {
    // 依側欄順序（組內依設定順序）；帳號已刪除的紀錄找不到帳號，略過。
    const games = config.accounts
      .filter((a) => a.region === region)
      .flatMap((a) => {
        const pid = pids.get(a.id);
        return pid === undefined ? [] : [{ label: accountLabel(a), pid }];
      });
    const act = actStatus ? HEADER_ACT[actStatus[region]] : null;
    return act || games.length ? [{ region, act, games }] : [];
  });
  const shown = JSON.stringify(groups);
  if (shown === runningStatusShown) return;
  runningStatusShown = shown;
  runningStatusBar.replaceChildren(
    ...groups.map(({ region, act, games }) => {
      const label = regionLabel(region);
      const group = h(
        "div",
        { class: "running-group" },
        act
          ? h("span", { class: `running-act ${act.cls}` }, `${label} ACT ${act.text}`)
          : h("span", { class: "running-act" }, label),
      );
      if (games.length) {
        group.append(
          h(
            "div",
            { class: "running-games" },
            ...games.map((g) =>
              h(
                "span",
                { class: "running-game" },
                h("span", { class: "running-game-name" }, g.label),
                h("span", { class: "muted" }, "PID"),
                h("span", { class: "running-pid" }, String(g.pid)),
              ),
            ),
          ),
        );
      }
      return group;
    }),
  );
  runningStatusBar.hidden = !groups.length;
}

/** 查 ACT 狀態與執行中遊戲，就地更新 header 與 ACT 子分頁。較晚發出的查詢優先；失敗沿用上次結果、不打擾使用者。 */
export function refreshRunningStatus(): void {
  const seq = ++runningStatusSeq;
  void api.getRunningStatus().then(
    (s) => {
      if (seq !== runningStatusSeq) return;
      actStatus = s.act;
      runningGames = s.games;
      paintRunningStatus();
      paintActStatus();
      // 側欄標記、設定檔「使用中」與訊息列原本只在重繪時更新；遊戲正常關閉沒有事件，靠這裡撤銷。
      if (config === undefined) return;
      const sync = syncRunning(launchNotice, shownMessage(), s.games, s.profilesInUse);
      config.accountsRunning = sync.accountsRunning;
      config.profilesInUse = sync.profilesInUse;
      // 每次都畫：config 也會被重繪或更新前的重讀換掉，不能拿它判斷畫面是否已同步；沒變時不動 DOM。
      paintAccountsRunning();
      paintProfilesInUse();
      if (sync.message !== null) say(sync.message);
    },
    (e: unknown) => {
      if (seq === runningStatusSeq) console.warn(`查詢執行狀態失敗：${errorText(e)}`);
    },
  );
}

function startRunningStatusTimer(): void {
  runningStatusTimer = window.setInterval(refreshRunningStatus, RUNNING_STATUS_POLL_MS);
}

/** 視窗可見時輪詢、隱藏時停；隱藏期間沒查，回到可見先補查一次。 */
function syncRunningStatusPolling(): void {
  if (document.visibilityState === "visible") {
    if (runningStatusTimer !== undefined) return;
    refreshRunningStatus();
    startRunningStatusTimer();
  } else if (runningStatusTimer !== undefined) {
    window.clearInterval(runningStatusTimer);
    runningStatusTimer = undefined;
  }
}

document.addEventListener("visibilitychange", syncRunningStatusPolling);
// 從別的視窗（例如剛關掉 ACT 或遊戲）回來時立即查。
window.addEventListener("focus", refreshRunningStatus);
// 啟動時的第一次查詢在 render()（讀到設定後）；之後視窗可見時定期查。
if (document.visibilityState === "visible") startRunningStatusTimer();
