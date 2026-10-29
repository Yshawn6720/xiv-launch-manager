import {
  api,
  channelRegion,
  channelVersion,
  installProgressId,
  regionChannel,
  regionLabel,
  CANCELLED_MESSAGE,
  UPDATE_REQUIRED_SUFFIX,
  type AccountView,
  type GameVersionCheck,
  type GlobalLanguage,
  type OtpMode,
  type PluginLoad,
  type Profile,
  type Region,
  type ServerState,
  type ServerStatus,
  type ProgressPayload,
} from "./api";
import { resetActTabRegion } from "./act";
import {
  accountLabel,
  type Child,
  config,
  errorText,
  field,
  fieldError,
  h,
  hhmm,
  openFolder,
  option,
  otpDialog,
  otpDialogTitle,
  otpInput,
  REGIONS,
  run,
  say,
  section,
} from "./app";
import {
  bindTarget,
  type DalamudChoice,
  dalamudVersionLabel,
  fillDalamudSelect,
  installedDalamudVersion,
  resetDalamudTabRegion,
  twTagLabel,
  twVersionSelect,
} from "./dalamud";
import {
  autoCheckTwGameUpdate,
  clearReloginPending,
  patchActions,
  patchStatus,
  reloginPending,
  startGameUpdate,
} from "./game-update";
import { otpPreview, secretView, type SecretState } from "./login-secret";
import { currentTab } from "./main";
import { newsGroup, paintNews } from "./news";
import { paintProgress, progressState, type ProgressState, progressView, type ProgressView } from "./progress";
import { lockIcon, moreMenu, renameInPlace } from "./widgets";

// ---------- 登入 ----------

/**
 * 跳出 OTP 對話框，回傳輸入的 OTP；取消（含 Esc）回傳 null。只用於國際服手動 OTP（繁中服見 `otpTypedHere`）。
 * 要在 Dalamud 準備好之後才呼叫，避免 30 秒 TOTP 在下載期間過期。
 */
function askOtp(label: string): Promise<string | null> {
  otpDialogTitle.textContent = `登入 ${label}`;
  otpInput.value = "";
  otpDialog.returnValue = "";
  // run() 會鎖住頁面上所有按鈕，含對話框裡的；這裡解鎖。
  otpDialog.querySelectorAll("button").forEach((b) => (b.disabled = false));
  // tsconfig lib 是 ES2020，沒有 Promise.withResolvers 型別，只能用 executor 形式。
  return new Promise((resolve) => {
    otpDialog.addEventListener(
      "close",
      () => resolve(otpDialog.returnValue === "login" ? otpInput.value.trim() : null),
      { once: true },
    );
    otpDialog.showModal();
    otpInput.focus();
  });
}

/** 各帳號最近一次登入的結果；只存在記憶體，重開程式即清空。 */
const lastResults = new Map<string, { text: string; ok: boolean; at: Date }>();

export function remember(accountId: string, text: string, ok: boolean): void {
  lastResults.set(accountId, { text, ok, at: new Date() });
}

/**
 * 繁中服帳號最近一次「成功」登入回應的剩餘遊戲時間文字（protocols §2.1）與取得時間；只存在記憶體，重開程式即清空。
 * 選中該帳號時顯示在頁首；之後登入失敗不清掉，附取得時間，與側欄「上次」那行的時間分得開。
 */
const playTimes = new Map<string, { text: string; at: Date }>();

/** 兩服伺服器狀態；null＝本次程式執行尚未取得。 */
const serverStatus: Record<Region, ServerState | null> = { global: null, tw: null };
let statusSeq = 0;
/** 帳號清單裡各區服的狀態列（line）與狀態文字（state），以及登入表單的國際服維修提示；狀態非同步回來後就地更新。 */
const serverLines = new Map<Region, { line: HTMLElement; state: HTMLElement }>();
let maintenanceNote: HTMLElement | null = null;

const SERVER_STATE_TEXT: Record<ServerState, string> = { open: "開放", maintenance: "維修中", unknown: "未知" };
const UNKNOWN_STATUS: ServerStatus = { global: "unknown", tw: "unknown" };

/** 有該服帳號或正在新增該服帳號時才顯示該服狀態。 */
function wantsServerStatus(region: Region): boolean {
  return draft?.region === region || config.accounts.some((a) => a.region === region);
}

function paintServerStatus(): void {
  for (const [region, { line, state }] of serverLines) {
    const s = serverStatus[region];
    line.hidden = !wantsServerStatus(region);
    state.textContent = s ? SERVER_STATE_TEXT[s] : "查詢中…";
    state.className = `server-state${s === "maintenance" ? " error" : s === "open" ? " ok" : ""}`;
  }
  // 只有國際服的維修會擋登入；台服狀態只用於顯示。
  if (maintenanceNote) maintenanceNote.hidden = draft?.region !== "global" || serverStatus.global !== "maintenance";
}

/** 不等結果：先用上次的狀態繪製，回來後只更新狀態與提示；兩服一次查詢，失敗視為未知。 */
export function refreshServerStatus(): void {
  if (!REGIONS.some(wantsServerStatus)) return;
  const seq = ++statusSeq;
  void api.getServerStatus().catch(() => UNKNOWN_STATUS).then((s) => {
    if (seq !== statusSeq) return;
    serverStatus.global = s.global;
    serverStatus.tw = s.tw;
    paintServerStatus();
  });
}

const LANGUAGE_TEXT: Record<GlobalLanguage, string> = { japanese: "日文", english: "英文", german: "德文", french: "法文" };

const OTP_MODE_TEXT: Record<OtpMode, string> = { none: "不使用", manual: "每次輸入", auto: "自動產生" };

/** 上次登入的帳號 id；只存 id，密碼一律不落地在前端。 */
const LAST_ACCOUNT_KEY = "xlm.lastAccountId";
/** 帳號清單「＋ 新增帳號」的選擇值；帳號 id 是 uuid，不會撞。 */
const NEW_ACCOUNT = "new";
/** 帳號清單選的帳號 id 或 NEW_ACCOUNT；null＝還沒決定（第一次繪製時決定）。 */
let selection: string | null = null;

/** 登入面板上尚未送出的輸入；重繪沿用，換帳號或儲存成功才依帳號重建。 */
export interface LoginDraft {
  /** 這份草稿屬於哪個帳號；null＝新增。 */
  accountId: string | null;
  region: Region;
  name: string;
  username: string;
  /** 只留在記憶體：記住密碼時經 saveAccount 送後端、儲存成功即清空；不記住時只交給該次登入。 */
  password: string;
  /** 「記住密碼」；與帳號的 rememberPassword 不同時，下一次儲存／登入一併送出。取消勾選＝刪除已存的密碼。 */
  rememberPassword: boolean;
  /** 已存密碼的欄位按了「更換」：顯示輸入框，留空仍沿用已存的。 */
  replacingPassword: boolean;
  otp: string;
  otpMode: OtpMode;
  otpSecret: string;
  /** 已存 OTP 金鑰的欄位按了「更換」。 */
  replacingOtpSecret: boolean;
  /** 已存 OTP 金鑰按了「清除」：下一次儲存／登入時刪除（同時輸入新金鑰則改存新的）。 */
  clearOtpSecret: boolean;
  language: GlobalLanguage;
  /** 新增模式的「Dalamud 設定檔」下拉值；已存帳號改下拉即套用，不經草稿。 */
  dalamud: string;
  /** 本次的 Dalamud 載入方式；只在有套用設定檔時有效。 */
  dalamudMode: DalamudMode;
  extraArgs: string;
  /** 「ACT 跟這個帳號」；新帳號預設勾選。 */
  actFollow: boolean;
  /** 「進階」（Dalamud 設定檔、本次載入方式、語言、ACT 跟這個帳號、額外啟動參數）是否展開；新帳號預設展開方便第一次設定，已存帳號預設收起。 */
  advancedOpen: boolean;
}

type DalamudMode = "normal" | "noThirdParty" | "noPlugin" | "off" | "force";
const DALAMUD_MODE_TEXT: Record<DalamudMode, string> = {
  normal: "正常",
  noThirdParty: "不載入第三方 plugin",
  noPlugin: "不載入任何 plugin",
  off: "不載入 Dalamud",
  // 只有國際服（goatcorp 才有遊戲版本比對）；送出前警告確認，送出後回到「正常」（protocols §1.5）。
  force: "強制載入（忽略遊戲版本）",
};

const FORCE_DALAMUD_WARNING =
  "強制載入會略過 Dalamud 的遊戲版本檢查。\n\n" +
  "遊戲剛改版、Dalamud 官方還沒支援新版本時硬注入，可能讓遊戲崩潰，或讓插件讀錯資料而行為異常。官方 XIVLauncher 不提供這個選項。\n\n" +
  "這個選擇只用於這一次登入，之後回到「正常」。仍要強制載入嗎？";

export let draft: LoginDraft | null = null;

type LoginField = "username" | "password" | "otp" | "otpSecret" | "dalamud" | "extraArgs";
interface LoginError {
  /** 要標示的欄位（id 為 `login-<field>`）；null＝顯示在「登入」鈕上方。 */
  field: LoginField | null;
  text: string;
}
/** 最近一次登入／儲存／套用失敗；只在下一次重繪顯示，之後清掉。 */
let loginError: LoginError | null = null;

/** 重建登入頁頁首的「⋯」選單（已存帳號才有）；遊戲更新狀態就地改變時用它換掉選單，不重繪整個面板。 */
export let loginMenu: (() => HTMLDetailsElement) | null = null;

/** 目前選的帳號；選擇失效（第一次繪製或帳號已刪除）時改選上次登入的帳號，再不行選第一個，沒有帳號就是新增。 */
function selectedAccount(): AccountView | null {
  const accounts = REGIONS.flatMap((r) => config.accounts.filter((a) => a.region === r));
  if (selection !== NEW_ACCOUNT && !accounts.some((a) => a.id === selection)) {
    const last = localStorage.getItem(LAST_ACCOUNT_KEY);
    selection = (accounts.find((a) => a.id === last) ?? accounts[0])?.id ?? NEW_ACCOUNT;
  }
  return accounts.find((a) => a.id === selection) ?? null;
}

function draftFor(a: AccountView | null): LoginDraft {
  return {
    accountId: a?.id ?? null,
    // 新帳號預設已設定遊戲路徑的區服。
    region: a?.region ?? (config.games.global === null && config.games.tw !== null ? "tw" : "global"),
    name: a?.name ?? "",
    username: a?.username ?? "",
    password: "",
    rememberPassword: a?.rememberPassword ?? true,
    replacingPassword: false,
    otp: "",
    otpMode: a?.otp ?? "none",
    otpSecret: "",
    replacingOtpSecret: false,
    clearOtpSecret: false,
    language: a?.language ?? "english",
    dalamud: "",
    dalamudMode: "normal",
    extraArgs: a?.extraArgs ?? "",
    actFollow: a?.actFollow ?? true,
    advancedOpen: a === null,
  };
}

/** 與後端儲存時相同的正規化：去頭尾、以單一空白分隔。 */
function normalizeExtraArgs(text: string): string {
  return text.trim().split(/\s+/).filter(Boolean).join(" ");
}

function draftChanged(a: AccountView, d: LoginDraft): boolean {
  return (
    d.name.trim() !== a.name ||
    d.username.trim() !== a.username ||
    d.otpMode !== a.otp ||
    d.language !== a.language ||
    d.rememberPassword !== a.rememberPassword ||
    normalizeExtraArgs(d.extraArgs) !== a.extraArgs ||
    d.actFollow !== a.actFollow ||
    // 不記住密碼時輸入的密碼只交給該次登入，不算變更。
    (d.rememberPassword && d.password !== "") ||
    (d.otpMode === "auto" && d.otpSecret.trim() !== "") ||
    d.clearOtpSecret
  );
}

/**
 * 手動 OTP 是否在主視窗輸入：國際服是；繁中服在登入確認視窗按「登入」前輸入（後端 `OtpSource::InDialog`），使用者
 * 在那個視窗停多久都不會送出過期的 code。
 */
const otpTypedHere = (d: LoginDraft): boolean => d.otpMode === "manual" && d.region === "global";

/** login＝要登入（沒有可用的已存密碼就要求輸入、檢查 OTP 碼格式）；否則只是儲存（新帳號記住密碼時要有密碼）。 */
function validateDraft(a: AccountView | null, d: LoginDraft, login: boolean): LoginError | null {
  if (!d.username.trim()) return { field: "username", text: "請輸入帳號" };
  const stored = d.rememberPassword && a?.hasPassword === true;
  if (!d.password && (login ? !stored : d.rememberPassword && !a)) return { field: "password", text: "請輸入密碼" };
  if (d.otpMode === "auto" && !d.otpSecret.trim() && (!a?.hasOtpSecret || d.clearOtpSecret)) {
    return { field: "otpSecret", text: "自動產生 OTP 需要 OTP 金鑰" };
  }
  const otp = d.otp.trim();
  if (login && otpTypedHere(d) && otp && !/^\d{6}$/.test(otp)) return { field: "otp", text: "OTP 碼為 6 位數字" };
  const bad = normalizeExtraArgs(d.extraArgs)
    .split(" ")
    .findIndex((t) => t !== "" && !/^[^=]+=.+$/.test(t));
  if (bad >= 0) return { field: "extraArgs", text: `第 ${bad + 1} 項不是 key=value 格式` };
  return null;
}

/**
 * 儲存草稿；新帳號再依下拉套用 Dalamud 設定檔。
 * 回傳帳號 id 與之後啟動要準備的設定檔 id。
 */
async function saveDraft(
  a: AccountView | null,
  d: LoginDraft,
  choices: Map<string, DalamudChoice>,
): Promise<{ id: string; profileId: string | null }> {
  const user = d.username.trim();
  // 沿用既有綁定；已失效的（設定檔不存在或區服不符）一併清掉，否則後端會拒絕儲存。
  const kept = a?.profileId && choices.has(a.profileId) ? a.profileId : null;
  const id = await api.saveAccount({
    id: a?.id ?? "",
    name: d.name.trim() || user,
    region: d.region,
    username: user,
    otp: d.otpMode,
    profileId: kept,
    language: d.language,
    // 不記住密碼：後端刪除已存的密碼，本次輸入的只交給登入。
    password: d.rememberPassword ? d.password || null : null,
    otpSecret: d.otpMode === "auto" ? d.otpSecret.trim() || null : null,
    clearOtpSecret: d.clearOtpSecret,
    rememberPassword: d.rememberPassword,
    extraArgs: d.extraArgs,
    actFollow: d.actFollow,
  });
  // 已寫進後端：之後就算套用失敗也要切到這個帳號，否則留在新增模式再按一次會建立重複帳號。
  selection = id;
  draft = null;
  if (a) return { id, profileId: kept };
  const target = bindTarget(choices, d.dalamud);
  if (target.kind === "none") return { id, profileId: null };
  try {
    return { id, profileId: await api.setAccountProfile(id, target) };
  } catch (e) {
    throw new Error(`已儲存帳號，但套用 Dalamud 設定檔失敗：${errorText(e)}`);
  }
}

function showLoginError(err: LoginError): void {
  loginError = err;
  say(err.text, "error");
  renderLogin();
  document.getElementById(err.field ? `login-${err.field}` : "login-submit")?.focus();
}

/** 登入面板的一欄：標籤在上、欄位在下，錯誤緊接在欄位下方。 */
export function loginField(text: string, control: HTMLElement, error: string | null, target: HTMLElement = control): HTMLElement {
  return h("div", { class: "login-field" }, ...field(text, control, target), ...fieldError(target, error));
}

/** 登入表單 OTP 預覽的計時器；重繪、換帳號、離開登入頁時停掉，同時只留一組。 */
let otpPreviewStop: (() => void) | null = null;

export function stopOtpPreview(): void {
  otpPreviewStop?.();
  otpPreviewStop = null;
}

/** 切換登入表單的帳號；換帳號會丟掉前一個帳號尚未送出的輸入。 */
function selectAccount(id: string): void {
  if (selection === id) return;
  selection = id;
  draft = null;
  // 「擴充功能」分頁 Dalamud 頁與 ACT 頁的區服子分頁回到跟著目前帳號的區服。
  resetDalamudTabRegion();
  resetActTabRegion();
  renderLogin();
  refreshServerStatus();
  if (selectedAccount()?.region === "tw") autoCheckTwGameUpdate();
  document.getElementById(`account-${id}`)?.focus();
}

/** 頭像縮寫：中日文等取第一個字；英數取前兩個詞的字首（只有一個詞就一個字母），大寫。 */
function initials(label: string): string {
  const words = label.split(/[\s._@-]+/).filter(Boolean);
  const [first = "", second = ""] = words.map((w) => Array.from(w)[0] ?? "");
  if (!first) return "?";
  if (!/[A-Za-z0-9]/.test(first)) return first;
  return (first + (/[A-Za-z0-9]/.test(second) ? second : "")).toUpperCase();
}

/** 帳號頭像：縮寫圓圈＋右下角區服色點。純裝飾（區服另有文字），不給輔助技術讀。 */
function avatar(a: AccountView): HTMLElement {
  return h("span", { class: `avatar ${a.region}`, "aria-hidden": "true" }, initials(accountLabel(a)));
}

/** 「新增帳號」的頭像位置：虛線圓圈裡的「＋」。 */
function addAvatar(): HTMLElement {
  return h("span", { class: "avatar avatar-add", "aria-hidden": "true" }, "＋");
}

/**
 * 頁首的「遊戲時間」標籤：時鐘圖示、「遊戲時間」、剩餘時間（粗體）與取得時間各一段，同一行不換行。完整說明放 title。
 */
function playTimeTag(p: { text: string; at: Date }): HTMLElement {
  const at = hhmm(p.at);
  return h(
    "span",
    { class: "play-time", title: `遊戲時間：${p.text}（${at} 登入時繁中服回報的值）` },
    h("span", { class: "play-time-icon", "aria-hidden": "true" }),
    h("span", {}, "遊戲時間"),
    h("strong", { class: "play-time-value" }, p.text),
    h("span", {}, `${at} 登入`),
  );
}

/** 左欄：兩服伺服器狀態與依區服分組的帳號清單（點選即切換右側登入表單的帳號），最下方是目前區服的官方消息。 */
function accountPane(selected: AccountView | null): HTMLElement {
  // 伺服器狀態：有該服分組時接在分組標題右側（標題已寫區服，這裡只寫「伺服器」）；
  // 還沒有該服帳號（新增該服帳號中）時單獨一行放在清單頂端。
  serverLines.clear();
  const serverLine = (region: Region, inGroup: boolean): HTMLElement => {
    const state = h("span");
    const line = h("div", { class: "server-line" }, h("span", {}, inGroup ? "伺服器" : regionLabel(region)), state);
    serverLines.set(region, { line, state });
    return line;
  };
  const standalone = REGIONS.filter((r) => !config.accounts.some((x) => x.region === r)).map((r) => serverLine(r, false));
  const item = (id: string, cls: string | null, ...children: Child[]): HTMLButtonElement => {
    const b = h("button", { type: "button", id: `account-${id}`, class: cls ? `account-item ${cls}` : "account-item" }, ...children);
    if (id === (selected?.id ?? NEW_ACCOUNT)) b.setAttribute("aria-current", "true");
    b.addEventListener("click", () => selectAccount(id));
    return b;
  };
  const accountItem = (x: AccountView): HTMLButtonElement => {
    const lines: HTMLElement[] = [
      h(
        "span",
        { class: "account-item-head" },
        h("span", { class: "account-item-name" }, accountLabel(x)),
        // 密碼未儲存＝登入時要輸入：名稱旁的開鎖圖示；文字給輔助技術，滑鼠停留也看得到。
        ...(x.hasPassword
          ? []
          : [h("span", { class: "account-item-flag", title: "密碼未儲存" }, lockIcon(false), h("span", { class: "visually-hidden" }, "密碼未儲存"))]),
        ...(config.accountsRunning.includes(x.id) ? [h("span", { class: "status-dot" }, "遊戲執行中")] : []),
      ),
      h("span", { class: "account-item-meta" }, x.username),
    ];
    const last = lastResults.get(x.id);
    if (last) {
      // 單行省略；完整訊息放 title，失敗時表單的錯誤區也有全文。
      const text = `上次：${last.text} ${hhmm(last.at)}`;
      lines.push(h("span", { class: `account-item-last ${last.ok ? "muted" : "error"}`, title: text }, text));
    }
    return item(x.id, null, avatar(x), h("span", { class: "account-item-text" }, ...lines));
  };
  const groups = REGIONS.flatMap((region) => {
    const list = config.accounts.filter((x) => x.region === region);
    if (!list.length) return [];
    const titleId = `accounts-${region}`;
    return [
      h(
        "div",
        { class: `account-group ${region}`, role: "group", "aria-labelledby": titleId },
        h(
          "div",
          { class: "account-group-head" },
          h("h3", { class: "account-group-title", id: titleId }, regionLabel(region)),
          serverLine(region, true),
        ),
        ...list.map(accountItem),
      ),
    ];
  });
  const guide =
    config.games.global === null && config.games.tw === null
      ? "還沒有帳號。先到「設定」分頁填好遊戲路徑，再在右側新增帳號。"
      : "還沒有帳號，請在右側新增帳號。";
  return h(
    "section",
    { class: "account-pane", "aria-label": "帳號" },
    ...standalone,
    ...(config.accounts.length
      ? [...groups, item(NEW_ACCOUNT, "account-add", addAvatar(), "新增帳號")]
      : [h("p", { class: "hint" }, guide)]),
    newsGroup(),
  );
}

export function renderLogin(): void {
  // 重繪（含換帳號）前先停掉上一個面板的 OTP 預覽，不留下多組計時器。
  stopOtpPreview();
  const a = selectedAccount();
  if (!draft || draft.accountId !== (a?.id ?? null)) draft = draftFor(a);
  const d = draft;
  const error = loginError;
  loginError = null;
  const errorFor = (f: LoginField): string | null => (error?.field === f ? error.text : null);
  const bound = a ? config.profiles.find((p) => p.id === a.profileId) : undefined;

  // 內容區頂部的頁首：選中的帳號（新增模式為「新增帳號」）與同一行的區服 badge（標題的屬性）一組，繁中服登入成功過的帳號
  // 再接「遊戲時間」標籤（protocols §2.1）；已存帳號最右邊是「⋯」選單：重新命名｜檢查遊戲更新或遊戲檔完整性（依區服）、
  // 開啟遊戲設定資料夾｜刪除帳號。新增模式沒有選單。水晶商城在側欄消息標題列（news.ts）。
  const title = h("h2", { class: "login-title", id: "login-title" }, a ? accountLabel(a) : "新增帳號");
  const regionBadge = h("span");
  const playTime = a ? playTimes.get(a.id) : undefined;
  const identity = h(
    "div",
    { class: "login-identity" },
    title,
    regionBadge,
    ...(playTime ? [playTimeTag(playTime)] : []),
  );
  const head = h("div", { class: "login-head" }, identity);
  loginMenu = null;
  if (a) {
    const rename = (): void => {
      const more = document.getElementById("login-more");
      if (!more) return;
      // 標題暫時藏起（form 的 aria-labelledby 仍指向它），原位換成同尺寸輸入框；留空退回帳號，與新增時一致。
      // 編輯期間「⋯」藏起，取消或名稱沒變時焦點回到它。
      renameInPlace(title, more, {
        value: a.name,
        fallback: a.username,
        label: "顯示名稱",
        className: "login-title-input",
        save: (next) => {
          // 不經草稿、只送名稱：密碼／OTP 等尚未送出的輸入留在草稿裡，重繪後照樣顯示。
          void run("重新命名帳號", async () => {
            await api.saveAccount({
              id: a.id,
              name: next,
              region: a.region,
              username: a.username,
              otp: a.otp,
              // 與 saveDraft 相同：已失效的綁定一併清掉，否則後端會拒絕儲存。
              profileId: a.profileId && choices.has(a.profileId) ? a.profileId : null,
              language: a.language,
              // null＝沿用已儲存的密碼與 OTP 金鑰。
              password: null,
              otpSecret: null,
              clearOtpSecret: false,
              rememberPassword: a.rememberPassword,
              extraArgs: a.extraArgs,
              actFollow: a.actFollow,
            });
            d.name = next;
            return `已改名為「${next}」`;
          }).then(() => document.getElementById("login-more")?.focus());
        },
      });
    };
    const remove = (): void => {
      if (confirm(`刪除帳號「${accountLabel(a)}」與其儲存的密碼、OTP 金鑰？`)) {
        void run("刪除帳號", () => api.deleteAccount(a.id));
      }
    };
    loginMenu = () =>
      moreMenu(
        "login-more",
        [
          [{ id: "login-rename", label: "重新命名", action: rename }],
          [
            ...patchActions(a.region),
            { label: "開啟遊戲設定資料夾", action: () => openFolder({ kind: "gameConfig", region: a.region }) },
          ],
          [{ label: "刪除帳號", action: remove, danger: true }],
        ],
        true,
      );
    head.append(loginMenu());
  }

  // 新增模式的顯示名稱；已存帳號在頁首「⋯」選單重新命名。
  const name = h("input", { id: "login-name", placeholder: "留空用帳號" });
  name.value = d.name;
  name.addEventListener("input", () => (d.name = name.value));

  // 只有新增模式能選區服；已存帳號的區服由頁首 badge 表達。原生 radio：方向鍵切換。
  const regionField = a
    ? null
    : h(
        "div",
        { class: "login-field" },
        h("span", { class: "login-label", id: "login-region-label" }, "區服"),
        h(
          "div",
          { class: "segmented", role: "radiogroup", "aria-labelledby": "login-region-label" },
          ...REGIONS.map((r) => {
            const radio = h("input", { type: "radio", name: "login-region", value: r });
            radio.checked = d.region === r;
            radio.addEventListener("change", () => {
              d.region = r;
              d.dalamud = "";
              d.dalamudMode = "normal";
              dalamudMode.value = "normal";
              paintRegion();
              refreshServerStatus();
              paintNews();
            });
            return h("label", { class: r }, radio, regionLabel(r));
          }),
        ),
      );

  const username = h("input", { id: "login-username", autocomplete: "off", spellcheck: "false" });
  username.value = d.username;
  username.addEventListener("input", () => (d.username = username.value));
  // 記住密碼：不勾＝每次登入輸入，只交給該次登入、不儲存；原本已存的密碼在下一次儲存／登入時刪除。
  // 已存密碼的「清除」走同一條路（取消勾選）：後端只能在不記住時刪除已存密碼。
  const password = h("input", { id: "login-password", type: "password", autocomplete: "off" });
  password.value = d.password;
  password.addEventListener("input", () => (d.password = password.value));
  const rememberBox = h("input", { id: "login-remember", type: "checkbox" });
  rememberBox.checked = d.rememberPassword;
  const resetPassword = (): void => {
    d.replacingPassword = false;
    d.password = "";
    password.value = "";
  };
  const passwordView = secretView(password, "密碼", errorFor("password"), {
    pendingText: "已儲存的密碼將於下次儲存或登入時刪除，之後每次登入時輸入",
    onReplace: () => {
      d.replacingPassword = true;
      paintPassword();
      password.focus();
    },
    onClear: () => {
      d.rememberPassword = false;
      rememberBox.checked = false;
      resetPassword();
      paintPassword();
      passwordView.focusUndo();
    },
    onCancel: () => {
      resetPassword();
      paintPassword();
      passwordView.focusStored("replace");
    },
    onUndo: () => {
      d.rememberPassword = true;
      rememberBox.checked = true;
      resetPassword();
      paintPassword();
      passwordView.focusStored("clear");
    },
  });
  function paintPassword(): void {
    // 已存＋記住＋沒輸入新密碼才是「已儲存」；重新勾回「記住密碼」時打過的字仍顯示在輸入框（會被儲存）。
    const state: SecretState = !a?.hasPassword
      ? "unsaved"
      : !d.rememberPassword
        ? "pending"
        : d.replacingPassword || d.password
          ? "replacing"
          : "stored";
    password.placeholder = !d.rememberPassword ? "每次登入輸入" : state === "replacing" ? "輸入新密碼" : "";
    passwordView.paint(state, d.rememberPassword ? "尚未儲存" : "不儲存，每次登入時輸入");
    if (state === "pending") rememberBox.setAttribute("aria-describedby", passwordView.pendingNote.id);
    else rememberBox.removeAttribute("aria-describedby");
  }
  rememberBox.addEventListener("change", () => {
    d.rememberPassword = rememberBox.checked;
    paintPassword();
  });
  paintPassword();
  // 「記住密碼」放在標籤列右側（同 OTP 方式），不另佔一行。
  const passwordField = h(
    "div",
    { class: "login-field" },
    h(
      "div",
      { class: "login-label-row" },
      h("label", { class: "login-label", for: password.id }, "密碼"),
      h("label", { class: "check" }, rememberBox, "記住密碼"),
    ),
    passwordView.row,
    password,
    ...passwordView.errors,
    passwordView.replacingNote,
    passwordView.pendingNote,
    passwordView.unsavedNote,
  );

  // OTP：標籤旁三選一。每次輸入→6 碼欄；自動產生→同位置換成 OTP 金鑰欄＋目前 OTP 預覽；不使用→不顯示欄位。
  const otp = h("input", {
    id: "login-otp",
    inputmode: "numeric",
    maxlength: "6",
    autocomplete: "one-time-code",
    placeholder: "留空則登入時再詢問",
  });
  otp.value = d.otp;
  otp.addEventListener("input", () => (d.otp = otp.value));
  const otpSecret = h("input", { id: "login-otpSecret", type: "password", autocomplete: "off", spellcheck: "false" });
  otpSecret.value = d.otpSecret;
  // 有輸入用輸入值預覽（不儲存）；沒輸入用已存的金鑰（已按「清除」就不用）。
  const preview = otpPreview(() => {
    const typed = d.otpSecret.trim();
    if (typed) return { accountId: a?.id ?? null, secret: typed };
    return a?.hasOtpSecret && !d.clearOtpSecret ? { accountId: a.id, secret: null } : null;
  });
  otpPreviewStop = () => preview.stop();
  /** 只在登入分頁、「自動產生」時跑預覽；其他時候停掉計時器。 */
  const syncPreview = (delay: number): void => {
    if (d.otpMode === "auto" && currentTab === "login") preview.refresh(delay);
    else preview.stop();
  };
  otpSecret.addEventListener("input", () => {
    d.otpSecret = otpSecret.value;
    syncPreview(300);
  });
  const resetSecret = (): void => {
    d.replacingOtpSecret = false;
    d.otpSecret = "";
    otpSecret.value = "";
  };
  const secretState = (): SecretState =>
    !a?.hasOtpSecret
      ? "unsaved"
      : d.clearOtpSecret
        ? "pending"
        : d.replacingOtpSecret || d.otpSecret
          ? "replacing"
          : "stored";
  const otpSecretView = secretView(otpSecret, "OTP 金鑰", errorFor("otpSecret"), {
    pendingText: "已儲存的 OTP 金鑰將於下次儲存或登入時刪除",
    onReplace: () => {
      d.replacingOtpSecret = true;
      paintSecret();
      otpSecret.focus();
    },
    onClear: () => {
      d.clearOtpSecret = true;
      resetSecret();
      paintSecret();
      syncPreview(0);
      otpSecretView.focusUndo();
    },
    onCancel: () => {
      resetSecret();
      paintSecret();
      syncPreview(0);
      otpSecretView.focusStored("replace");
    },
    onUndo: () => {
      d.clearOtpSecret = false;
      resetSecret();
      paintSecret();
      syncPreview(0);
      // 「復原」在非「自動產生」時也看得到；那時狀態列是藏起來的，焦點留在 OTP 方式。
      if (d.otpMode === "auto") otpSecretView.focusStored("clear");
      else document.querySelector<HTMLInputElement>('input[name="login-otp-mode"]:checked')?.focus();
    },
  });
  function paintSecret(): void {
    const state = secretState();
    otpSecret.placeholder =
      state === "pending" ? "輸入新金鑰，或改用其他 OTP 方式" : state === "replacing" ? "輸入新的 base32 金鑰" : "base32 金鑰";
    otpSecretView.paint(state, "尚未儲存");
    // 已儲存：目前 OTP 併進「已儲存」列（更換／清除之前）；其他狀態排在金鑰欄與狀態小字下方。
    if (state === "stored") otpSecretView.row.insertBefore(preview.box, otpSecretView.row.querySelector(".secret-actions"));
    else autoPart.append(preview.box);
  }
  const otpLabel = h("label", { class: "login-label" });
  const codePart = h("div", { class: "login-field-part" }, otp, ...fieldError(otp, errorFor("otp")));
  // 自動產生：金鑰欄、狀態小字由上往下排；目前 OTP 由 paintSecret 依狀態放進「已儲存」列或排在最下方。
  const autoPart = h(
    "div",
    { class: "login-field-part" },
    otpSecretView.row,
    otpSecret,
    ...otpSecretView.errors,
    otpSecretView.replacingNote,
    otpSecretView.unsavedNote,
  );
  paintSecret();
  // 只有尚未儲存金鑰時說明金鑰存放處；已儲存時「已儲存」列本身已表達。
  const autoHint = h("p", { class: "field-hint" }, "金鑰存在 Windows 認證管理員，登入時自動產生 OTP");
  const dialogHint = h("p", { class: "field-hint" }, "登入時在「繁中服登入確認」視窗輸入，按「登入」前填當下的 OTP 碼");
  const paintOtp = (): void => {
    const input = d.otpMode === "auto" ? otpSecret : otpTypedHere(d) ? otp : null;
    codePart.hidden = input !== otp;
    dialogHint.hidden = d.otpMode !== "manual" || input === otp;
    autoPart.hidden = input !== otpSecret;
    autoHint.hidden = input !== otpSecret || a?.hasOtpSecret === true;
    otpLabel.textContent = input === otpSecret ? "OTP 金鑰" : input === otp ? "OTP 碼" : "OTP";
    if (input) otpLabel.htmlFor = input.id;
    else otpLabel.removeAttribute("for");
    syncPreview(0);
  };
  const otpModes = h(
    "div",
    { class: "segmented segmented-sm", role: "radiogroup", "aria-label": "OTP 使用方式" },
    ...(Object.keys(OTP_MODE_TEXT) as OtpMode[]).map((m) => {
      const radio = h("input", { type: "radio", name: "login-otp-mode", value: m });
      radio.checked = d.otpMode === m;
      radio.addEventListener("change", () => {
        d.otpMode = m;
        paintOtp();
      });
      return h("label", {}, radio, OTP_MODE_TEXT[m]);
    }),
  );
  paintOtp();
  // 「將刪除」的說明放在各模式共用的位置：清除後改用其他 OTP 方式時仍看得到、可復原。
  const otpField = h(
    "div",
    { class: "login-field" },
    h("div", { class: "login-label-row" }, otpLabel, otpModes),
    codePart,
    autoPart,
    otpSecretView.pendingNote,
    autoHint,
    dialogHint,
  );

  // 已存帳號：選了就套用（保留原本的即時套用）；新帳號：儲存後才套用。
  const dalamud = h("select", { id: "login-dalamud" });
  let choices = new Map<string, DalamudChoice>();
  const modeOptions = (Object.keys(DALAMUD_MODE_TEXT) as DalamudMode[]).map((m) =>
    option(m, DALAMUD_MODE_TEXT[m], d.dalamudMode === m),
  );
  const dalamudMode = h("select", { id: "login-dalamud-mode" }, ...modeOptions);
  dalamudMode.addEventListener("change", () => {
    d.dalamudMode = dalamudMode.value as DalamudMode;
    paintAdvanced();
  });
  // 設定檔、語言、設定檔資訊與本次載入方式都收在「進階」（首頁不直接顯示第三方載入選項）；
  // 本次載入方式不是「正常」時，收起的「進階」摘要會寫出來，Dalamud 相關錯誤訊息也會指到這裡。
  // 與「進階」裡其他欄位同一種版式（標籤在上、同高的下拉），和台服的 Dalamud 版本並排。
  const dalamudModeField = loginField("本次載入方式", dalamudMode, null);
  const dalamudLine = h("p", { class: "meta login-dalamud-info" });
  const applied = (): boolean => (a ? bound !== undefined : d.dalamud !== "");
  /** 目前套用的設定（已存帳號為綁定的設定檔；新帳號為下拉選的）；沒有＝不載入。 */
  const currentChoice = (): DalamudChoice | undefined => choices.get(a ? (a.profileId ?? "") : d.dalamud);
  dalamud.addEventListener("change", () => {
    if (!a) {
      d.dalamud = dalamud.value;
      paintDalamud();
      return;
    }
    // 偵測到的舊資料交給後端沿用或新增設定檔後綁定，前端不先建立。
    const value = dalamud.value;
    const chosen = choices.get(value);
    const label = accountLabel(a);
    void run(`變更「${label}」的 Dalamud 設定檔`, async () => {
      try {
        await api.setAccountProfile(a.id, bindTarget(choices, value));
      } catch (e) {
        loginError = { field: "dalamud", text: errorText(e) };
        throw e;
      }
      return chosen ? `「${label}」改為套用「${chosen.name}」` : `「${label}」改為不載入 Dalamud`;
    }).then(() => document.getElementById("login-dalamud")?.focus());
  });

  const language = h(
    "select",
    { id: "login-language" },
    ...(Object.keys(LANGUAGE_TEXT) as GlobalLanguage[]).map((l) => option(l, LANGUAGE_TEXT[l], d.language === l)),
  );
  language.addEventListener("change", () => {
    d.language = language.value as GlobalLanguage;
    paintAdvanced();
    paintNews();
  });
  const languageField = loginField("語言", language, null);
  languageField.classList.add("login-language");

  const extraArgs = h("input", {
    id: "login-extraArgs",
    autocomplete: "off",
    spellcheck: "false",
    placeholder: "例：SYS.ScreenMode=2（空白分隔的 key=value）",
  });
  extraArgs.value = d.extraArgs;
  const actFollow = h("input", { id: "login-actFollow", type: "checkbox" });
  actFollow.checked = d.actFollow;
  // 「進階」摘要列：收起時也看得到目前的設定檔、本次載入方式（不是正常才列）、語言（只有國際服）、ACT 不跟（沒勾才列）與額外參數（有設才列）。
  // 已存帳號綁定的設定檔已不存在時註明（側欄不再列 Dalamud，這裡是收起時唯一看得到的地方）。
  const advancedValue = h("span", { class: "login-advanced-value" });
  const paintAdvanced = (): void => {
    const c = currentChoice();
    const parts = [c ? `Dalamud：${c.name}` : a?.profileId ? "Dalamud 設定檔不存在" : "不載入 Dalamud"];
    if (applied() && d.dalamudMode !== "normal") parts.push(`本次：${DALAMUD_MODE_TEXT[d.dalamudMode]}`);
    if (d.region === "global") parts.push(LANGUAGE_TEXT[d.language]);
    if (!d.actFollow) parts.push("ACT 不跟");
    const args = normalizeExtraArgs(d.extraArgs);
    if (args) parts.push(`額外參數 ${args}`);
    advancedValue.textContent = ` · ${parts.join("、")}`;
  };
  /** 套用的設定改變（換區服、新帳號換下拉）時：設定檔資訊、本次載入方式與「進階」摘要一起更新。 */
  const paintDalamud = (): void => {
    const c = currentChoice();
    dalamudLine.replaceChildren(...(c?.profile ? profileDetails(c.profile, c.path, a !== null) : []));
    dalamudLine.hidden = !c?.profile;
    dalamudModeField.hidden = !applied();
    // 強制載入只對國際服有意義；換區服時 d.dalamudMode 已重設為正常。
    const force = modeOptions.find((o) => o.value === "force");
    if (force) force.hidden = force.disabled = d.region !== "global";
    paintAdvanced();
  };
  actFollow.addEventListener("change", () => {
    d.actFollow = actFollow.checked;
    paintAdvanced();
  });
  extraArgs.addEventListener("input", () => {
    d.extraArgs = extraArgs.value;
    paintAdvanced();
  });
  const extraArgsError = errorFor("extraArgs");
  const dalamudError = errorFor("dalamud");

  const gameHint = h("p", { class: "login-alert" });
  maintenanceNote = h("p", { class: "login-alert callout" }, "國際服維修中，現在登入會被擋下。");
  const formError = error && error.field === null ? h("p", { class: "login-alert", id: "login-error" }, error.text) : null;
  const submit = h("button", { id: "login-submit", class: "primary login-submit", type: "submit" }, "登入");
  if (formError) submit.setAttribute("aria-describedby", formError.id);
  loginProgress = progressView("登入進度", true);
  paintProgress(loginProgress, launching ? launchProgress : null);
  // 遊戲更新的狀態與操作（更新中斷標記、待套用更新、更新進度、國際服完整性檢查的進度與結果）在「登入」按鈕上方，
  // 依區服就地更新；「檢查遊戲更新」「檢查遊戲檔完整性」在頁首「⋯」選單。
  const patchBox = h("div", { id: "patch-status", class: "patch-status" });

  // 只存設定、不登入（例如先改好密碼或 OTP 方式）。
  const save = h("button", { id: "login-save", class: "secondary", type: "button" }, "僅儲存");

  // 在「Dalamud」小標題底下，標籤不再重複「Dalamud」。
  const dalamudField = loginField("設定檔", dalamud, dalamudError);
  dalamudField.classList.add("login-dalamud");

  // 先依區服填好「Dalamud 設定檔」下拉，設定檔資訊與台服版本切換才找得到目前套用的設定。
  paintRegion();
  const choice = a?.profileId ? choices.get(a.profileId) : undefined;
  const advanced = h(
    "details",
    { class: "editor-advanced login-advanced" },
    h("summary", {}, "進階", advancedValue),
    // 展開的內容是單層的三段，各段以小標題＋分隔線開頭：Dalamud（設定檔、語言、設定檔資訊、版本與本次載入方式）、ACT、啟動參數。
    h(
      "div",
      { class: "editor-advanced-body login-advanced-body" },
      h(
        "div",
        { class: "adv-section" },
        h("h3", { class: "adv-heading" }, "Dalamud"),
        // 設定檔與語言（只有國際服）同一列；隱藏的欄不佔位。
        h("div", { class: "login-row login-dalamud-row" }, dalamudField, languageField),
        dalamudLine,
        // 設定檔本身的版本（台服，存進設定檔）先於只影響這次登入的載入方式；兩欄都不顯示時整列不佔位（CSS）。
        h("div", { class: "login-row" }, ...(a && choice?.profile ? twVersionField(choice.profile) : []), dalamudModeField),
      ),
      h(
        "div",
        { class: "adv-section login-act-follow" },
        h("h3", { class: "adv-heading" }, "ACT"),
        h("label", { class: "check" }, actFollow, "ACT 跟這個帳號"),
        h(
          "p",
          { class: "field-hint" },
          "登入時讓 ACT 切到這個遊戲，並依「擴充功能 › ACT」的設定自動開啟。同一服開兩個帳號時，小號可取消勾選，免得搶走主號的 ACT；ACT 跟著的遊戲關掉時，會切回同一服仍開著、有勾選的帳號中最晚開的那個（本程式要開著）。",
        ),
      ),
      h(
        "div",
        { class: "adv-section" },
        h("h3", { class: "adv-heading" }, "啟動參數"),
        loginField("額外參數（選填）", extraArgs, extraArgsError),
      ),
    ),
  );
  // 任一欄有錯時一定展開，欄位與錯誤才看得到、聚焦得到。
  advanced.open = d.advancedOpen || extraArgsError !== null || dalamudError !== null;
  advanced.addEventListener("toggle", () => (d.advancedOpen = advanced.open));

  // 登入／儲存期間連輸入框一起停用，避免送出後才改到的內容被重繪蓋掉。
  // 「登入」列不放進 fieldset：停用的 fieldset 會連下載的「取消」一起停用；「登入」鈕由 run() 鎖住。
  const fields = h(
    "fieldset",
    { class: "login-fields" },
    head,
    // 帳號與密碼同一列（窄視窗由 CSS 退回上下排）；OTP 單獨一列；其餘短欄位並排。
    // 新增模式：區服決定語言、設定檔清單與更新區，排在顯示名稱前面。
    ...(regionField ? [h("div", { class: "login-row" }, regionField, loginField("顯示名稱（選填）", name, null))] : []),
    h("div", { class: "login-row login-credentials" }, loginField("帳號", username, errorFor("username")), passwordField),
    otpField,
    advanced,
    gameHint,
    maintenanceNote,
  );
  const lockFields = (locked: boolean): void => {
    fields.disabled = locked;
  };
  const form = h(
    "form",
    { class: "login-panel", "aria-labelledby": "login-title" },
    fields,
    // 遊戲更新區、錯誤與「登入」緊接在登入欄位後，預設視窗不捲動就看得到。patchBox 不成框，子元素直接排進這一列。
    h(
      "div",
      { class: "login-submit-bar" },
      ...(formError ? [formError] : []),
      patchBox,
      h("div", { class: "login-buttons" }, submit, save),
      loginProgress.box,
    ),
  );

  /** 區服相關的部分：新增模式切換區服時就地更新，不重繪整個面板。 */
  function paintRegion(): void {
    regionBadge.className = `badge ${d.region}`;
    regionBadge.textContent = regionLabel(d.region);
    languageField.hidden = d.region !== "global";
    gameHint.hidden = config.games[d.region] !== null;
    gameHint.textContent = `尚未設定${regionLabel(d.region)}遊戲路徑，請先到「設定」分頁填寫。`;
    choices = fillDalamudSelect(dalamud, d.region, a ? a.profileId : d.dalamud || null);
    if (!a && d.dalamud && !choices.has(d.dalamud)) {
      // 上次選的舊資料已不在清單（例如已被其他帳號沿用成設定檔）：改回不載入。
      d.dalamud = "";
      choices = fillDalamudSelect(dalamud, d.region, null);
    }
    paintDalamud();
    // 手動 OTP 的輸入位置依區服而定（otpTypedHere）。
    paintOtp();
    patchBox.replaceChildren(...patchStatus(d.region));
    paintServerStatus();
  }

  save.addEventListener("click", () => {
    const err = validateDraft(a, d, false);
    if (err) {
      showLoginError(err);
      return;
    }
    lockFields(true);
    void run("儲存帳號", async () => {
      try {
        await saveDraft(a, d, choices);
      } catch (e) {
        loginError = { field: null, text: errorText(e) };
        throw e;
      }
    }).finally(() => lockFields(false));
  });

  form.addEventListener("submit", (ev) => {
    ev.preventDefault();
    const err = validateDraft(a, d, true);
    if (err) {
      showLoginError(err);
      return;
    }
    const label = d.name.trim() || d.username.trim();
    const mode = applied() ? d.dalamudMode : "normal";
    if (mode === "force") {
      if (!confirm(FORCE_DALAMUD_WARNING)) return;
      // 每次都要重選：送出後就回到「正常」，下次登入不沿用。
      d.dalamudMode = "normal";
      dalamudMode.value = "normal";
      paintAdvanced();
    }
    const withoutDalamud = mode === "off";
    const plugins: PluginLoad = mode === "noThirdParty" ? "noThirdParty" : mode === "noPlugin" ? "none" : "all";
    const versionCheck: GameVersionCheck = mode === "force" ? "skip" : "enforce";
    const manual = otpTypedHere(d);
    const typedOtp = manual ? d.otp.trim() || null : null;
    // 不記住密碼：本次輸入的密碼直接交給登入，不經 saveAccount。
    const typedPassword = d.rememberPassword ? null : d.password || null;
    const changed = !a || draftChanged(a, d);
    // OTP 與不記住的密碼都只用這一次；失敗時要重填。
    d.otp = "";
    if (!d.rememberPassword) d.password = "";
    lockFields(true);
    submit.textContent = "登入中…";
    /** 登入時版本檢查取得更新清單（已存成 pending）的區服：登入結束後直接開始更新。 */
    let updateRegion: Region | null = null;
    void run(`登入 ${label}`, async () => {
      let id = a?.id ?? null;
      try {
        const target = a && !changed ? { id: a.id, profileId: bound?.id ?? null } : await saveDraft(a, d, choices);
        id = target.id;
        localStorage.setItem(LAST_ACCOUNT_KEY, id);
        launching = { accountId: id, region: d.region };
        const profileId = withoutDalamud ? null : target.profileId;
        // 用後端最新的設定與佔用狀態：別的帳號可能剛開遊戲；新帳號沿用偵測到的舊資料時，設定檔是後端剛建立的。
        const fresh = profileId ? await api.getConfig() : null;
        const profile = fresh?.profiles.find((p) => p.id === profileId);
        // 第一次共用正在使用的設定檔：先講明會互相覆蓋並建議複製（protocols §3），在下載前問，取消就不登入。
        if (profile && !profile.shareConfirmed && fresh?.profilesInUse.includes(profile.id)) {
          const ok = confirm(
            `Dalamud 設定檔「${profile.name}」正被另一個遊戲使用。\n\n` +
              "兩個遊戲共用同一個設定檔時，兩邊改的設定（含插件設定）會互相覆蓋：後存的蓋掉先存的。\n" +
              "雙開建議改用複本：到「擴充功能 › Dalamud」按這個設定檔的「複製」，再讓這個帳號改用複本。\n\n" +
              "仍要共用嗎？按「確定」直接登入，這個設定檔之後不再詢問。",
          );
          if (!ok) {
            remember(id, "已取消", true);
            return "已取消";
          }
          await api.saveProfile({ ...profile, shareConfirmed: true });
        }
        // 先把 Dalamud 下載好再問 OTP、再登入：下載可能要好幾分鐘，OTP 會在下載期間過期。後端登入時沿用這次準備好的
        // 結果（同版本且本體重核通過），不再準備一次。
        if (profile) {
          say("準備 Dalamud…");
          await api.installDalamud(channelRegion(profile.channel), channelVersion(profile.channel), versionCheck);
        }
        let code = typedOtp;
        if (code === null && manual) {
          clearLaunchProgress();
          code = await askOtp(label);
          if (code === null) {
            remember(id, "已取消", true);
            return "已取消";
          }
        }
        // 準備 Dalamud 只到這裡；之後的登入、啟動遊戲與注入（遊戲初始化慢時可能等數十秒）不能還顯示「準備 Dalamud…」，
        // 訊息列回到「登入」，各階段顯示在登入面板的進度。
        say(`登入 ${label}…`);
        const r = await api.launchAccount(id, code, typedPassword, withoutDalamud, plugins, versionCheck);
        // 狀態與訊息都用「・」接版本，避免和 Dalamud 標籤裡的括號疊在一起。
        const loaded = r.dalamudVersion
          ? `・${dalamudVersionLabel(d.region, r.dalamudVersion, await installedDalamudVersion(d.region, r.dalamudVersion))}`
          : "";
        remember(id, `已啟動${loaded}`, true);
        if (r.playTime !== null) playTimes.set(id, { text: r.playTime, at: new Date() });
        if (d.region === "global") clearReloginPending();
        const started = `已啟動 ${label}（pid ${r.pid}）${loaded}`;
        // 遊戲已啟動，但 ACT 沒開起來、ACT 連動沒更新，或外部包沒套用：附上原因，不用錯誤樣式。
        const notes = [r.actWarning, r.actTargetWarning, r.packWarning].filter((w): w is string => w !== null);
        return notes.length ? { text: [started, ...notes].join("；"), kind: "" } : started;
      } catch (e) {
        const raw = errorText(e);
        const cancelled = raw === CANCELLED_MESSAGE;
        if (!cancelled && raw.endsWith(UPDATE_REQUIRED_SUFFIX)) {
          // 需要更新不是登入錯誤：不標在表單上，由更新區接手顯示進度（國際服安裝在受保護位置時，套用前照樣跳 UAC）。
          updateRegion = d.region;
          if (id) remember(id, raw, false);
          return { text: `${regionLabel(d.region)}遊戲需要更新，開始更新`, kind: "" };
        }
        // 國際服更新後的重新登入失敗：註明更新本身已完成，不讓使用者以為是更新失敗。
        const text = !cancelled && d.region === "global" && reloginPending ? `更新已完成、重新登入失敗：${raw}` : raw;
        if (id) remember(id, text, cancelled);
        // 取消下載不是錯誤：只在訊息列顯示，不標在表單上。
        if (!cancelled) loginError = { field: null, text };
        throw text === raw ? e : new Error(text);
      } finally {
        // 在 run() 重繪之前清掉，重建的面板就不會再畫進度。
        launching = null;
        launchProgress = null;
      }
    }).finally(() => {
      lockFields(false);
      // run() 已解鎖並重繪；更新完成後不自動重新登入（國際服提示再按「登入」）。
      if (updateRegion) startGameUpdate(updateRegion);
    });
  });

  // 右側內容區自己捲動，登入表單直接鋪在內容區上（不另成卡片）。
  section("login").replaceChildren(h("div", { class: "login-layout" }, accountPane(a), h("div", { class: "login-main" }, form)));
  // 狀態列是 accountPane() 新建的，要在掛上後再畫一次。
  paintServerStatus();
}

/**
 * 「進階」裡套用中設定檔的資訊（一行小字；名稱、插件數與找不到資料夾已在下拉選項）：已安裝版本；
 * links＝附「開啟資料夾」「開啟 log」。還沒成為設定檔的舊資料（新帳號選了偵測到的資料）沒有這行。
 */
function profileDetails(p: Profile, path: string, links: boolean): HTMLElement[] {
  const info = config.profileInfo.find((i) => i.id === p.id);
  const version = h(
    "span",
    {},
    info?.installedVersion
      ? dalamudVersionLabel(channelRegion(p.channel), info.installedVersion, info.installedDalamudVersion)
      : "尚未下載，登入時會自動下載",
  );
  if (!links) return [version];
  const openDir = h("button", { class: "ghost", type: "button", title: path }, "開啟資料夾");
  openDir.addEventListener("click", () => openFolder({ kind: "profile", id: p.id }));
  const openLogs = h("button", { class: "ghost", type: "button" }, "開啟 log");
  openLogs.addEventListener("click", () => openFolder({ kind: "profileLogs", id: p.id }));
  // 兩個按鈕自成一組（CSS 拿掉組前的「·」），換行時不留孤立的分隔點。
  return [version, h("span", { class: "login-dalamud-links" }, openDir, openLogs)];
}

/**
 * 「進階」裡的台服 Dalamud 版本切換（與其他欄位同版式：標籤在上，影響範圍與錯誤在下）：選了就存進設定檔，
 * 下次登入生效；國際服沒有版本可選，回傳空。
 */
function twVersionField(p: Profile): HTMLElement[] {
  const version = channelVersion(p.channel);
  if (!version) return [];
  const { select, error } = twVersionSelect("login-dalamud-version", version.kind === "pinned" ? version.tag : "");
  select.addEventListener("change", () => {
    const tag = select.value;
    void run(`切換「${p.name}」的 Dalamud 版本`, async () => {
      await api.saveProfile({ ...p, channel: regionChannel("tw", tag ? { kind: "pinned", tag } : { kind: "latest" }) });
      return `「${p.name}」改用${tag ? `固定 ${twTagLabel(tag)}` : "最新版"}`;
    }).then(() => document.getElementById(select.id)?.focus());
  });
  const sharing = config.accounts.filter((x) => x.profileId === p.id);
  return [
    h(
      "div",
      { class: "login-field" },
      ...field("版本", select),
      ...(sharing.length > 1 ? [h("p", { class: "field-hint" }, `切換版本會影響：${sharing.map(accountLabel).join("、")}`)] : []),
      error,
    ),
  ];
}

/** 開啟程式時游標放在接下來要填的欄位；都齊了就停在「登入」。 */
export function focusLoginStart(): void {
  const d = draft;
  if (!d) return;
  const a = config.accounts.find((x) => x.id === d.accountId);
  const target = !d.username
    ? "login-username"
    : !(d.rememberPassword && a?.hasPassword)
      ? "login-password"
      : otpTypedHere(d)
        ? "login-otp"
        : "login-submit";
  document.getElementById(target)?.focus();
}

/**
 * 進行中的登入：登入面板承接 accountId 為帳號 id 或該區服 install:<region> 的進度事件。
 * run() 期間按鈕全鎖，同時至多一筆。進度狀態放這裡而非 DOM，因為 render() 會重建面板。
 */
export let launching: { accountId: string; region: Region } | null = null;
let launchProgress: ProgressState | null = null;
/** renderLogin() 時重建：登入面板內的進度元素。 */
let loginProgress: ProgressView | null = null;

/** 這次登入的進度事件（帳號本身或它區服的 Dalamud 下載）畫在登入面板；不是這次登入的回傳 false。 */
export function paintLaunchProgress(p: ProgressPayload): boolean {
  if (!launching || (p.accountId !== launching.accountId && p.accountId !== installProgressId(launching.region))) {
    return false;
  }
  launchProgress = progressState(p);
  if (loginProgress) paintProgress(loginProgress, launchProgress);
  return true;
}

/** 遊戲提早結束：正看著該帳號時標在表單上，下一次重繪顯示。 */
export function flagExitedEarly(accountId: string, text: string): void {
  if (selectedAccount()?.id === accountId) loginError = { field: null, text };
}

/** 等手動 OTP 時收起面板進度，避免殘留的下載進度看起來像還在跑。 */
function clearLaunchProgress(): void {
  launchProgress = null;
  if (loginProgress) paintProgress(loginProgress, null);
}
