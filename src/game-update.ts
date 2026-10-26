import {
  api,
  regionLabel,
  type ConfigView,
  type GameUpdateProgress,
  type IntegrityOutcome,
  type PatchState,
  type Region,
} from "./api";
import { config, h, run, runsActive } from "./app";
import { draft, loginMenu, renderLogin } from "./login";
import {
  MB,
  paintProgress,
  progressState,
  type ProgressState,
  progressView,
  type ProgressView,
  resetSpeed,
} from "./progress";
import { type MenuItem } from "./widgets";

// ---------- 遊戲更新 ----------

/** 各區服的更新中斷標記與待套用更新；render() 時重讀，null＝讀取失敗（原因已顯示在訊息列）。 */
let patchState: PatchState | null = null;
/**
 * 進行中的遊戲更新：進度畫在登入面板的更新區。run() 期間按鈕全鎖，同時至多一筆。
 * 進度狀態放這裡而非 DOM，因為 render() 會重建面板。
 */
export let gameUpdating: { region: Region; progress: ProgressState | null } | null = null;
/** renderLogin() 時重建：更新中的進度元素。 */
let gameUpdateProgress: ProgressView | null = null;
/** 國際服更新完成、尚未重新登入成功：之後的登入失敗要註明更新本身已完成。只存記憶體。 */
export let reloginPending = false;

export function setPatchState(next: PatchState | null): void {
  patchState = next;
}

/** 國際服重新登入成功：之後的登入失敗不再註明更新已完成。 */
export function clearReloginPending(): void {
  reloginPending = false;
}

const mbText = (bytes: number): string => `${(bytes / MB).toFixed(1)} MB`;

function gameUpdateState(p: GameUpdateProgress): ProgressState {
  switch (p.kind) {
    case "download":
      return progressState({ stage: `下載 patch ${p.patchIndex + 1}/${p.patchCount}`, done: p.done, total: p.total });
    case "verify":
      resetSpeed();
      return { text: `核對 patch ${p.patchIndex + 1}/${p.patchCount}`, done: 0, total: null, cancellable: true };
    case "elevate":
      resetSpeed();
      return { text: "等待系統管理員權限（請在 Windows 的使用者帳戶控制視窗按「是」）", done: 0, total: null, cancellable: false };
    case "apply":
      // 套用開始後不能取消：中途停下會留下半更新的遊戲檔。
      return {
        ...progressState({ stage: `套用 patch ${p.patchIndex + 1}/${p.patchCount}`, done: p.done, total: p.total }),
        cancellable: false,
      };
  }
}

export function startGameUpdate(region: Region): void {
  const label = regionLabel(region);
  gameUpdating = { region, progress: null };
  if (region === "global") reloginPending = false;
  // 先畫出進度與「取消」，run() 再鎖住其餘按鈕。
  renderLogin();
  gameUpdateProgress?.cancel.focus();
  void run(`更新${label}遊戲`, async () => {
    try {
      await api.updateGame(region, (p) => {
        if (!gameUpdating) return;
        gameUpdating.progress = gameUpdateState(p);
        if (gameUpdateProgress) paintProgress(gameUpdateProgress, gameUpdating.progress);
      });
    } finally {
      // 在 run() 重繪之前清掉，重建的面板就回到一般狀態。
      gameUpdating = null;
    }
    if (region === "global") {
      // 國際服更新後以新的登入結果啟動：使用者再按一次「登入」（手動 OTP 重輸、自動 OTP 重新產生）。
      reloginPending = true;
      return "國際服遊戲更新完成，請重新啟動以登入";
    }
    return `${regionLabel("tw")}遊戲更新完成，可以登入`;
  }).then(() => document.getElementById("login-submit")?.focus());
}

function clearPatchDirty(region: Region): void {
  const label = regionLabel(region);
  const ok = confirm(
    `確認已用${label}官方啟動器修復遊戲檔？\n\n清除更新中斷標記後，本程式將允許${label}啟動與更新。尚未修復就清除，可能以損毀的遊戲檔啟動。`,
  );
  if (!ok) return;
  void run(`清除${label}更新中斷標記`, async () => {
    await api.clearPatchDirty(region);
    return `已清除${label}更新中斷標記，可以登入`;
  }).then(() => document.getElementById("login-submit")?.focus());
}

function checkTwGameUpdate(): void {
  const label = regionLabel("tw");
  void run(`檢查${label}遊戲更新`, async () => {
    const r = await api.checkGameUpdate("tw");
    switch (r.kind) {
      case "upToDate":
        return `${label}遊戲已是最新版本`;
      case "updateAvailable":
        return `${label}遊戲需要更新：${r.patchCount} 個 patch，共 ${mbText(r.totalBytes)}`;
      case "loginRequired":
        return `${label}需要登入才能取得更新清單`;
    }
  }).then(() => document.getElementById("login-more")?.focus());
}

/** 台服遊戲更新自動檢查的最小間隔：啟動時查一次，之後選到台服帳號時距上次成功檢查超過此間隔才再查。 */
const TW_AUTO_CHECK_INTERVAL_MS = 30 * 60 * 1000;
/** 最近一次台服自動檢查成功的時間（Date.now()）；null＝本次執行還沒成功查過。失敗不記，下次選到台服帳號再試。 */
let twAutoCheckedAt: number | null = null;
/** 台服自動檢查進行中：同時至多一筆。 */
let twAutoChecking = false;

/**
 * 台服遊戲更新自動檢查（台服查版本不需登入）：後端把清單存成 pending，這裡重讀 patchState 並就地重畫更新區，
 * 「登入」上方就出現「需要更新」與「更新」。不走 run()（不鎖面板、不顯示訊息）、不自動下載；失敗靜默，
 * 原因可用手動「檢查遊戲更新」看到。有 run() 進行中（登入、遊戲更新、Dalamud 安裝、完整性檢查、程式更新等
 * 都會佔後端 launch_lock）時不觸發。
 */
export function autoCheckTwGameUpdate(): void {
  // 啟動時讀設定失敗的話 config 還沒有值。
  if (((config as ConfigView | undefined)?.games.tw ?? null) === null || twAutoChecking || runsActive > 0) return;
  if (twAutoCheckedAt !== null && Date.now() - twAutoCheckedAt < TW_AUTO_CHECK_INTERVAL_MS) return;
  twAutoChecking = true;
  void (async () => {
    await api.checkGameUpdate("tw");
    twAutoCheckedAt = Date.now();
    const state = await api.getPatchState();
    // 檢查期間開始的 run() 結束時會重讀 patchState 並重繪；這裡就地重畫會蓋掉它鎖住的按鈕。
    if (runsActive > 0) return;
    patchState = state;
    repaintPatchStatus();
  })()
    .catch((e: unknown) => console.warn("台服遊戲更新自動檢查失敗", e))
    .finally(() => (twAutoChecking = false));
}

/**
 * 就地重畫登入面板的更新區，頁首「⋯」選單也換成新的（檢查項目跟著更新狀態）；不重繪整個面板，不打斷正在輸入的欄位。
 * 改名中（「⋯」藏起）不換選單，否則改名結束時焦點回不到選單。
 */
function repaintPatchStatus(): void {
  const box = document.getElementById("patch-status");
  if (box && draft) box.replaceChildren(...patchStatus(draft.region));
  const more = document.getElementById("login-more");
  const menu = more?.parentElement;
  if (!loginMenu || !more || more.hidden || !(menu instanceof HTMLDetailsElement)) return;
  const next = loginMenu();
  next.open = menu.open;
  const focused = menu.contains(document.activeElement);
  menu.replaceWith(next);
  if (focused) document.getElementById("login-more")?.focus();
}

/**
 * 登入面板「登入」列上方的遊戲更新區（依區服）：更新中斷標記與確認清除；待套用更新與「更新」；
 * 國際服更新完成後的重新登入提示；更新進度；國際服完整性檢查的進度或最近結果。未設定遊戲路徑時為空。
 * 「檢查遊戲更新」「檢查遊戲檔完整性」在頁首「⋯」選單（patchActions）。
 */
export function patchStatus(region: Region): HTMLElement[] {
  gameUpdateProgress = null;
  integrityProgress = null;
  if (config.games[region] === null) return [];
  const label = regionLabel(region);
  const state = patchState?.[region];
  const job = gameUpdating?.region === region ? gameUpdating : null;
  const out: HTMLElement[] = [];
  if (state?.dirty) {
    // 標記存在時後端拒絕啟動與更新；只提供修復後的確認清除。
    const fixed = h("button", { id: "patch-dirty-clear", class: "secondary", type: "button" }, "我已用官方啟動器修復");
    fixed.addEventListener("click", () => clearPatchDirty(region));
    out.push(
      h(
        "div",
        { class: "patch-note warn" },
        h("p", {}, "上次更新中斷，遊戲檔可能損毀，請先用官方啟動器修復"),
        fixed,
      ),
    );
  } else if (state?.pending) {
    const { patchCount, totalBytes } = state.pending;
    const update = h("button", { id: "patch-update", class: "primary", type: "button" }, job ? "更新中…" : "更新");
    update.disabled = job !== null;
    update.addEventListener("click", () => startGameUpdate(region));
    out.push(
      h(
        "div",
        { class: "patch-note" },
        h("p", {}, `${label}需要更新：${patchCount} 個 patch、總大小 ${mbText(totalBytes)}`),
        update,
      ),
    );
  } else if (region === "global" && reloginPending) {
    out.push(h("div", { class: "patch-note" }, h("p", {}, "更新完成，請重新啟動以登入（再按一次「登入」）")));
  }
  if (job) {
    gameUpdateProgress = progressView(`${label}遊戲更新進度`, true);
    paintProgress(gameUpdateProgress, job.progress ?? { text: "準備更新…", done: 0, total: null, cancellable: true });
    out.push(gameUpdateProgress.box);
  } else if (region === "global") {
    out.push(...integrityStatus());
  }
  return out;
}

/**
 * 頁首「⋯」選單的遊戲檔檢查（依區服；未設定遊戲路徑或該服更新中時沒有）：台服沒有更新中斷標記、待套用更新時
 * 「檢查遊戲更新」；國際服「檢查遊戲檔完整性」（檢查中停用並顯示「檢查中…」）。
 */
export function patchActions(region: Region): MenuItem[] {
  if (config.games[region] === null || gameUpdating?.region === region) return [];
  if (region === "tw") {
    const state = patchState?.tw;
    return state?.dirty || state?.pending ? [] : [{ id: "patch-check", label: "檢查遊戲更新", action: checkTwGameUpdate }];
  }
  return [
    {
      id: "integrity-check",
      label: integrityCheck ? "檢查中…" : "檢查遊戲檔完整性",
      action: startIntegrityCheck,
      disabled: integrityCheck !== null,
      title: "以 goatcorp（第三方）提供的對照逐檔核對遊戲檔；只檢查、不修復，有問題請用官方啟動器修復",
    },
  ];
}

// ---------- 完整性檢查 ----------

/** 結果清單每類最多列幾個路徑；其餘只顯示數量。 */
const INTEGRITY_LIST_LIMIT = 10;

/** 進行中的國際服完整性檢查（progress＝最近一次進度）；patchStatus() 依此畫進度。 */
export let integrityCheck: { progress: ProgressState | null } | null = null;
/** patchStatus() 時重建：檢查中的進度元素。 */
let integrityProgress: ProgressView | null = null;
/** 最近一次完成的檢查結果；重繪沿用，只存記憶體。 */
let integrityResult: IntegrityOutcome | null = null;

function startIntegrityCheck(): void {
  integrityCheck = { progress: null };
  integrityResult = null;
  renderLogin();
  integrityProgress?.cancel.focus();
  void run("檢查國際服遊戲檔", async () => {
    let outcome: IntegrityOutcome;
    try {
      outcome = await api.checkGameIntegrity((p) => {
        if (!integrityCheck) return;
        integrityCheck.progress = progressState(p);
        if (integrityProgress) paintProgress(integrityProgress, integrityCheck.progress);
      });
    } finally {
      integrityCheck = null;
    }
    integrityResult = outcome;
    if (outcome.kind === "noReference") {
      return { text: `國際服 ${outcome.gameVersion} 沒有完整性對照資料，無法檢查`, kind: "" };
    }
    const bad = outcome.missing.length + outcome.mismatched.length;
    return bad === 0
      ? `國際服遊戲檔全部正常（${outcome.checked} 個檔案）`
      : { text: `國際服遊戲檔有 ${bad} 個問題，請用國際服官方啟動器修復`, kind: "error" };
  }).then(() => document.getElementById("login-more")?.focus());
}

/** 對照的 key（如 `\game\ffxiv_dx11.exe`）→ 相對安裝根目錄的路徑。 */
const integrityPath = (key: string): string => key.replace(/^[\\/]+/, "");

function integrityList(title: string, keys: string[]): HTMLElement[] {
  if (!keys.length) return [];
  const shown = keys.slice(0, INTEGRITY_LIST_LIMIT);
  const rest = keys.length - shown.length;
  return [
    h("p", { class: "integrity-list-title" }, `${title} ${keys.length} 個${rest > 0 ? `（列出前 ${shown.length} 個）` : ""}`),
    h("ul", { class: "integrity-list" }, ...shown.map((k) => h("li", { class: "path" }, integrityPath(k)))),
    ...(rest > 0 ? [h("p", { class: "hint" }, `…另有 ${rest} 個`)] : []),
  ];
}

/** 最近一次結果：全部正常／缺檔與不符（列出前幾個相對路徑並提示官方修復）／沒有對照資料（404）。 */
function integrityResultView(r: IntegrityOutcome): HTMLElement {
  if (r.kind === "noReference") {
    return h(
      "div",
      { class: "integrity-result", role: "status" },
      h("p", {}, `國際服版本 ${r.gameVersion} 沒有完整性對照資料（對照來源回 404），無法檢查。`),
    );
  }
  const bad = r.missing.length + r.mismatched.length;
  if (bad === 0) {
    return h(
      "div",
      { class: "integrity-result ok", role: "status" },
      h("p", {}, `全部正常：${r.checked} 個檔案與對照相符（版本 ${r.gameVersion}）。`),
    );
  }
  return h(
    "div",
    { class: "integrity-result error", role: "status" },
    h(
      "p",
      {},
      `發現問題：缺檔 ${r.missing.length}、不符 ${r.mismatched.length}（共核對 ${r.checked} 個檔案，版本 ${r.gameVersion}）。請用國際服官方啟動器修復。`,
    ),
    ...integrityList("缺檔", r.missing),
    ...integrityList("不符", r.mismatched),
  );
}

/** 國際服完整性檢查（台服沒有對照來源）的進度或最近結果，接在更新區；唯讀、不修復。 */
function integrityStatus(): HTMLElement[] {
  if (integrityCheck) {
    integrityProgress = progressView("國際服完整性檢查進度", false);
    paintProgress(integrityProgress, integrityCheck.progress ?? { text: "下載完整性對照…", done: 0, total: null, cancellable: true });
    return [integrityProgress.box];
  }
  return integrityResult ? [integrityResultView(integrityResult)] : [];
}
