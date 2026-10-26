import { getVersion } from "@tauri-apps/api/app";
import { listen } from "@tauri-apps/api/event";
import {
  api,
  installProgressId,
  GAME_EXITED_EARLY_EVENT,
  PROGRESS_EVENT,
  type GameExitedEarlyPayload,
  type ProgressPayload,
} from "./api";
import { setActData } from "./act";
import { applyTheme, config, errorText, runsActive, say, setConfig, setDetected } from "./app";
import { backgroundUpdateCheck, paintAppUpdate, UPDATE_CHECK_TICK_MS, versionButton } from "./app-update";
import { renderCharacters, setCharacters } from "./characters";
import { installing, installProgress } from "./dalamud";
import { renderExtensions } from "./extensions";
import { autoCheckTwGameUpdate, setPatchState } from "./game-update";
import {
  flagExitedEarly,
  focusLoginStart,
  paintLaunchProgress,
  refreshServerStatus,
  remember,
  renderLogin,
  stopOtpPreview,
} from "./login";
import { paintProgress, progressState } from "./progress";
import { paintRunningStatus, refreshRunningStatus } from "./running-status";
import { renderSettings } from "./settings";

// ---------- 主流程 ----------

export let currentTab = "login";

export async function render(): Promise<void> {
  // 與設定並行偵測舊資料、讀更新狀態、ACT 設定與 ACT 連動狀態（已下載的 Dalamud 隨設定一起回傳）；失敗只顯示訊息，不擋其他繪製。
  const detecting = api.detectDalamudData().then(
    (list) => list,
    (e: unknown) => {
      say(errorText(e), "error");
      return null;
    },
  );
  const patching = api.getPatchState().then(
    (s) => s,
    (e: unknown) => {
      say(errorText(e), "error");
      return null;
    },
  );
  const acting = api.getActSettings().then(
    (s) => s,
    (e: unknown) => {
      say(errorText(e), "error");
      return null;
    },
  );
  const linking = api.getActLinkStatus().then(
    (s) => s,
    (e: unknown) => {
      say(errorText(e), "error");
      return null;
    },
  );
  const choosing = api.listActChoices().then(
    (list) => list,
    (e: unknown) => {
      say(errorText(e), "error");
      return null;
    },
  );
  // 角色只在「角色」分頁讀取（要掃兩服的角色資料夾與備份）；切到該分頁時 showTab() 會重繪。
  const listingCharacters =
    currentTab === "characters"
      ? api.listCharacters().then(
          (list) => list,
          (e: unknown) => {
            say(errorText(e), "error");
            return null;
          },
        )
      : null;
  try {
    setConfig(await api.getConfig());
  } catch (e) {
    say(errorText(e), "error");
    return;
  }
  applyTheme(config.ui.theme);
  setDetected(await detecting);
  setPatchState(await patching);
  setActData(await acting, await linking, await choosing);
  renderLogin();
  renderExtensions();
  renderSettings();
  if (listingCharacters) {
    setCharacters(await listingCharacters);
    renderCharacters();
  }
  paintAppUpdate();
  if (currentTab === "login") refreshServerStatus();
  // 帳號名稱可能改了，先用上次的結果重畫；每個動作（含登入啟動、開啟 ACT）結束後 run() 都會重繪，這裡順便重查。
  paintRunningStatus();
  refreshRunningStatus();
}

/** 切換分頁並重繪。 */
export async function showTab(tab: string): Promise<void> {
  currentTab = tab;
  // 離開登入頁立即停掉 OTP 預覽；不必等 render() 取完設定。
  if (tab !== "login") stopOtpPreview();
  document.querySelectorAll<HTMLButtonElement>("nav button").forEach((b) => {
    const active = b.dataset.tab === tab;
    b.classList.toggle("active", active);
    if (active) b.setAttribute("aria-current", "page");
    else b.removeAttribute("aria-current");
  });
  document.querySelectorAll<HTMLElement>("main > section").forEach((s) => {
    s.hidden = s.id !== `tab-${tab}`;
  });
  // main 是捲動容器；換分頁從頂端開始。
  document.querySelector("main")?.scrollTo(0, 0);
  await render();
}

document.querySelectorAll<HTMLButtonElement>("nav button").forEach((button) => {
  button.addEventListener("click", () => void showTab(button.dataset.tab ?? "login"));
});

void listen<ProgressPayload>(PROGRESS_EVENT, (event) => {
  const p = event.payload;
  if (paintLaunchProgress(p)) return;
  if (installing && p.accountId === installProgressId(installing.region)) {
    installing.progress = progressState(p);
    if (installProgress) paintProgress(installProgress, installing.progress);
  }
  // 其他事件（例如取消後晚到的）已沒有對應的畫面，略過。
});

// 登入已回成功、背景觀察到遊戲提早結束：後端已撤銷執行狀態並叫回視窗。訊息與同步回錯相同，記在該帳號的「上次」、
// 訊息列，正看著該帳號時也標在表單上。
void listen<GameExitedEarlyPayload>(GAME_EXITED_EARLY_EVENT, (event) => {
  const { accountId, message: text } = event.payload;
  remember(accountId, text, false);
  say(text, "error");
  refreshRunningStatus();
  // 有進行中的動作時不重繪（會換掉 run() 鎖住的按鈕），表單也不標（那時看的可能是別的帳號）；動作結束的重繪會畫出「上次」。
  if (runsActive > 0) return;
  flagExitedEarly(accountId, text);
  renderLogin();
});

void render().then(() => {
  focusLoginStart();
  // 啟動後查一次台服遊戲更新（已設定台服遊戲路徑時）；查到的更新只顯示，不自動下載。
  autoCheckTwGameUpdate();
});

// 標題列的程式版本：每次啟動取一次，不會變。版本號本身就是「檢查更新」按鈕；讀不到版本時改顯示「檢查更新」。
void getVersion().then(
  (ver) => {
    versionButton.textContent = `v${ver}`;
    versionButton.setAttribute("aria-label", `v${ver}，檢查更新`);
    versionButton.hidden = false;
  },
  (e: unknown) => {
    versionButton.textContent = "檢查更新";
    versionButton.hidden = false;
    say(`讀取程式版本失敗：${errorText(e)}`, "error");
  },
);

// 啟動時查一次，之後定期與視窗重新取得焦點時，距上次超過間隔才再查。
backgroundUpdateCheck();
window.setInterval(backgroundUpdateCheck, UPDATE_CHECK_TICK_MS);
window.addEventListener("focus", backgroundUpdateCheck);
