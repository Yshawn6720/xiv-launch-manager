import { getVersion } from "@tauri-apps/api/app";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { listen } from "@tauri-apps/api/event";
import {
  api,
  channelRegion,
  channelVersion,
  installProgressId,
  isRegionInstall,
  regionChannel,
  regionLabel,
  CANCELLED_MESSAGE,
  GAME_EXITED_EARLY_EVENT,
  PROGRESS_EVENT,
  UPDATE_REQUIRED_SUFFIX,
  type ActConfig,
  type AfterLaunch,
  type ActChange,
  type ActChoice,
  type ActLinkState,
  type ActLinkStatus,
  type ActPluginStatus,
  type ActPluginVersions,
  type ActStatus,
  type ActStatusByRegion,
  type RunningGame,
  type AccountView,
  type BindTarget,
  type ConfigView,
  type DetectedDalamudData,
  type FolderTarget,
  type GameUpdateProgress,
  type GameVersionCheck,
  type GlobalLanguage,
  type InstalledVersion,
  type IntegrityOutcome,
  type LicenseDoc,
  type OtpMode,
  type PatchState,
  type PluginLoad,
  type Progress,
  type GameExitedEarlyPayload,
  type ProgressPayload,
  type Profile,
  type Region,
  type ServerState,
  type ServerStatus,
  type Theme,
  type NewsItem,
  type YanmucorpVersion,
} from "./api";

// ---------- 主題（設定 › 介面） ----------

/**
 * 主題的鏡像（localStorage）：程式一開就套用、不等讀設定（第一次 render 之前），深淺不會閃一下；render() 讀到
 * 設定後以設定為準。只有強制深色／淺色時設 data-theme；跟隨系統時拿掉，styles.css 的 color-scheme 跟系統。
 */
const THEME_KEY = "xlm.theme";
const THEMES: readonly Theme[] = ["system", "dark", "light"];

function applyTheme(theme: Theme): void {
  if (theme === "system") delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = theme;
  localStorage.setItem(THEME_KEY, theme);
}

const mirroredTheme = localStorage.getItem(THEME_KEY);
applyTheme(THEMES.find((t) => t === mirroredTheme) ?? "system");

type Child = Node | string;

function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: Partial<Record<string, string>> = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (value !== undefined) el.setAttribute(key, value);
  }
  el.append(...children);
  return el;
}

function option(value: string, label: string, selected: boolean): HTMLOptionElement {
  const o = h("option", { value }, label);
  o.selected = selected;
  return o;
}

let fieldSeq = 0;

/** 表單的一列：label 以 for 關聯 target（預設就是 control 本身），點標籤即聚焦欄位。 */
function field(text: string, control: HTMLElement, target: HTMLElement = control): HTMLElement[] {
  if (!target.id) target.id = `field-${++fieldSeq}`;
  return [h("label", { for: target.id }, text), control];
}

function section(id: string): HTMLElement {
  const el = document.getElementById(`tab-${id}`);
  if (!el) throw new Error(`缺少 #tab-${id}`);
  return el;
}

const message = document.getElementById("message") as HTMLDivElement;
const otpDialog = document.getElementById("otp-dialog") as HTMLDialogElement;
const otpDialogTitle = document.getElementById("otp-dialog-title") as HTMLHeadingElement;
const otpInput = document.getElementById("otp-input") as HTMLInputElement;

function say(text: string, kind: "ok" | "error" | "" = ""): void {
  message.textContent = text;
  message.className = kind;
}

function errorText(e: unknown): string {
  return typeof e === "string" ? e : e instanceof Error ? e.message : String(e);
}

/** 動作完成時的訊息：字串為成功樣式；完成但結果不是成功（例如檢查發現問題）時指定樣式。 */
type RunResult = string | { text: string; kind: "ok" | "error" | "" };

/** 進行中的 run() 數；背景動作（例如台服遊戲更新自動檢查）據此避開正在鎖住面板的動作。 */
let runsActive = 0;

/** 執行一個會改狀態的動作：期間鎖住按鈕與下拉選單（下載進度的「取消」除外），結束後重繪。 */
async function run(label: string, action: () => Promise<RunResult | void>): Promise<void> {
  runsActive++;
  // 只鎖原本可用的；本來就停用的（例如依區服停用的欄位）結束後維持停用。
  const locked = Array.from(
    document.querySelectorAll<HTMLButtonElement | HTMLSelectElement>("button:not(.progress-cancel), select"),
  ).filter((c) => !c.disabled);
  locked.forEach((c) => (c.disabled = true));
  say(`${label}…`);
  try {
    const done = (await action()) ?? `${label}完成`;
    if (typeof done === "string") say(done, "ok");
    else say(done.text, done.kind);
  } catch (e) {
    const text = errorText(e);
    // 使用者自己按「取消」的下載不算錯誤。
    say(text, text === CANCELLED_MESSAGE ? "" : "error");
  } finally {
    resetSpeed();
    // render() 只重建 section 內容；nav 等常駐按鈕要在這裡還原。
    locked.forEach((c) => (c.disabled = false));
    try {
      await render();
    } finally {
      runsActive--;
    }
  }
}

let config: ConfigView;
/** 台服可下載的版本（新到舊）；同一次執行成功抓到一次就不再抓。 */
let twVersions: string[] | null = null;
let twVersionsLoad: Promise<string[]> | null = null;
/** 最近一次讀取台服版本清單失敗的原因；不自動重試，版本下拉旁的「重試」可手動重試。 */
let twVersionsError: string | null = null;
/** 偵測到的舊啟動器 Dalamud 資料；null＝偵測失敗（錯誤已顯示在訊息列）。 */
let detected: DetectedDalamudData[] | null = null;

const REGIONS: Region[] = ["global", "tw"];

/** 開資料夾不走 run()：不鎖按鈕、不重繪，失敗只顯示訊息。 */
function openFolder(target: FolderTarget): void {
  api.openFolder(target).catch((e: unknown) => say(errorText(e), "error"));
}

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

function remember(accountId: string, text: string, ok: boolean): void {
  lastResults.set(accountId, { text, ok, at: new Date() });
}

function hhmm(d: Date): string {
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

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
function refreshServerStatus(): void {
  if (!REGIONS.some(wantsServerStatus)) return;
  const seq = ++statusSeq;
  void api.getServerStatus().catch(() => UNKNOWN_STATUS).then((s) => {
    if (seq !== statusSeq) return;
    serverStatus.global = s.global;
    serverStatus.tw = s.tw;
    paintServerStatus();
  });
}

// 舊資料可能存了空名稱，退回帳號。
const accountLabel = (a: AccountView): string => a.name || a.username;

const LANGUAGE_TEXT: Record<GlobalLanguage, string> = { japanese: "日文", english: "英文", german: "德文", french: "法文" };

const OTP_MODE_TEXT: Record<OtpMode, string> = { none: "不使用", manual: "每次輸入", auto: "自動產生" };

/** 上次登入的帳號 id；只存 id，密碼一律不落地在前端。 */
const LAST_ACCOUNT_KEY = "xlm.lastAccountId";
/** 帳號清單「＋ 新增帳號」的選擇值；帳號 id 是 uuid，不會撞。 */
const NEW_ACCOUNT = "new";
/** 帳號清單選的帳號 id 或 NEW_ACCOUNT；null＝還沒決定（第一次繪製時決定）。 */
let selection: string | null = null;

/** 登入面板上尚未送出的輸入；重繪沿用，換帳號或儲存成功才依帳號重建。 */
interface LoginDraft {
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

let draft: LoginDraft | null = null;

type LoginField = "username" | "password" | "otp" | "otpSecret" | "dalamud" | "extraArgs";
interface LoginError {
  /** 要標示的欄位（id 為 `login-<field>`）；null＝顯示在「登入」鈕上方。 */
  field: LoginField | null;
  text: string;
}
/** 最近一次登入／儲存／套用失敗；只在下一次重繪顯示，之後清掉。 */
let loginError: LoginError | null = null;

/** 重建登入頁頁首的「⋯」選單（已存帳號才有）；遊戲更新狀態就地改變時用它換掉選單，不重繪整個面板。 */
let loginMenu: (() => HTMLDetailsElement) | null = null;

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

/** 欄位下方的錯誤：標記 target 為 aria-invalid 並以 aria-describedby 關聯；沒有錯誤時為空。 */
function fieldError(target: HTMLElement, error: string | null): HTMLElement[] {
  if (!error) return [];
  const id = `${target.id}-error`;
  target.setAttribute("aria-invalid", "true");
  target.setAttribute("aria-describedby", id);
  return [h("p", { class: "field-error", id }, error)];
}

/** 登入面板的一欄：標籤在上、欄位在下，錯誤緊接在欄位下方。 */
function loginField(text: string, control: HTMLElement, error: string | null, target: HTMLElement = control): HTMLElement {
  return h("div", { class: "login-field" }, ...field(text, control, target), ...fieldError(target, error));
}

const SVG_NS = "http://www.w3.org/2000/svg";

/** 鎖頭圖示（currentColor）：closed＝已儲存，open＝未儲存／將刪除。純裝飾，旁邊一定有文字。 */
function lockIcon(closed: boolean): SVGSVGElement {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("class", "lock-icon");
  svg.setAttribute("aria-hidden", "true");
  const shackle = document.createElementNS(SVG_NS, "path");
  shackle.setAttribute("d", closed ? "M5 7V5a3 3 0 0 1 6 0v2" : "M5 7V5a3 3 0 0 1 5.8-1.1");
  shackle.setAttribute("fill", "none");
  shackle.setAttribute("stroke", "currentColor");
  shackle.setAttribute("stroke-width", "1.6");
  shackle.setAttribute("stroke-linecap", "round");
  const body = document.createElementNS(SVG_NS, "rect");
  for (const [k, v] of Object.entries({ x: "3", y: "7", width: "10", height: "7", rx: "1.5", fill: "currentColor" })) {
    body.setAttribute(k, v);
  }
  svg.append(shackle, body);
  return svg;
}

/** 「⋯」圖示（currentColor）：三個圓點。純裝飾，旁邊一定有給輔助技術的文字。 */
function moreIcon(): SVGSVGElement {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("class", "more-icon");
  svg.setAttribute("aria-hidden", "true");
  for (const cx of ["3", "8", "13"]) {
    const dot = document.createElementNS(SVG_NS, "circle");
    for (const [k, v] of Object.entries({ cx, cy: "8", r: "1.5", fill: "currentColor" })) dot.setAttribute(k, v);
    svg.append(dot);
  }
  return svg;
}

/** stored＝已儲存（唯讀狀態列）、replacing＝按了「更換」、pending＝按了「清除」待刪除、unsaved＝尚未儲存。 */
type SecretState = "stored" | "replacing" | "pending" | "unsaved";

interface SecretView {
  /** 已儲存時取代輸入框的唯讀狀態列。 */
  row: HTMLElement;
  errors: HTMLElement[];
  replacingNote: HTMLElement;
  pendingNote: HTMLElement;
  unsavedNote: HTMLElement;
  paint(state: SecretState, unsavedText: string): void;
  focusStored(which: "replace" | "clear"): void;
  focusUndo(): void;
}

/**
 * 密碼／OTP 金鑰欄的儲存狀態，用圖示＋顏色＋文字表達，不靠 placeholder：
 * 已儲存＝鎖頭、遮罩、「已儲存」與「更換」「清除」；更換中＝輸入框＋「取消」（Esc 同）；
 * 待刪除＝紅字說明＋「復原」；尚未儲存＝一般輸入框＋小字說明。
 */
function secretView(
  input: HTMLInputElement,
  what: string,
  error: string | null,
  o: { pendingText: string; onReplace: () => void; onClear: () => void; onCancel: () => void; onUndo: () => void },
): SecretView {
  // 「OTP 金鑰」開頭是英文，動詞後補空格。
  const named = (verb: string): string => (/^[A-Za-z]/.test(what) ? `${verb} ${what}` : `${verb}${what}`);
  const replace = h("button", { class: "ghost", type: "button", "aria-label": named("更換") }, "更換");
  const clear = h("button", { class: "ghost danger", type: "button", "aria-label": named("清除已儲存的") }, "清除");
  replace.addEventListener("click", o.onReplace);
  clear.addEventListener("click", o.onClear);
  const row = h(
    "div",
    { class: "secret-stored", role: "group", "aria-label": what },
    lockIcon(true),
    h("span", { class: "secret-mask", "aria-hidden": "true" }, "••••••••"),
    h("span", { class: "badge saved" }, "已儲存"),
    h("span", { class: "secret-actions" }, replace, clear),
  );
  const cancel = h("button", { class: "link", type: "button" }, "取消");
  cancel.addEventListener("click", o.onCancel);
  const replacingNote = h(
    "p",
    { class: "secret-note", id: `${input.id}-replacing` },
    `${named("正在更換")}；按 Esc 或`,
    cancel,
    "保留已儲存的",
  );
  const undo = h("button", { class: "ghost", type: "button" }, "復原");
  undo.addEventListener("click", o.onUndo);
  const pendingNote = h(
    "p",
    { class: "secret-note pending", id: `${input.id}-pending` },
    lockIcon(false),
    h("span", {}, o.pendingText),
    undo,
  );
  const unsavedText = h("span");
  const unsavedNote = h("p", { class: "secret-note", id: `${input.id}-unsaved` }, lockIcon(false), unsavedText);
  const errors = fieldError(input, error);
  let state: SecretState = "unsaved";
  input.addEventListener("keydown", (ev) => {
    if (state !== "replacing" || ev.isComposing || ev.key !== "Escape") return;
    ev.preventDefault();
    o.onCancel();
  });
  return {
    row,
    errors,
    replacingNote,
    pendingNote,
    unsavedNote,
    paint(next, text) {
      state = next;
      row.hidden = next !== "stored";
      input.hidden = next === "stored";
      replacingNote.hidden = next !== "replacing";
      pendingNote.hidden = next !== "pending";
      unsavedNote.hidden = next !== "unsaved";
      unsavedText.textContent = text;
      const note = next === "replacing" ? replacingNote : next === "pending" ? pendingNote : next === "unsaved" ? unsavedNote : null;
      const ids = [...errors.map((e) => e.id), ...(note ? [note.id] : [])];
      if (ids.length) input.setAttribute("aria-describedby", ids.join(" "));
      else input.removeAttribute("aria-describedby");
    },
    focusStored: (which) => (which === "replace" ? replace : clear).focus(),
    focusUndo: () => undo.focus(),
  };
}

/** 登入表單 OTP 預覽的計時器；重繪、換帳號、離開登入頁時停掉，同時只留一組。 */
let otpPreviewStop: (() => void) | null = null;

function stopOtpPreview(): void {
  otpPreviewStop?.();
  otpPreviewStop = null;
}

interface OtpPreviewView {
  box: HTMLElement;
  /** 依目前的金鑰來源重新取碼；delay＞0 為輸入中的 debounce。 */
  refresh(delay: number): void;
  stop(): void;
}

/**
 * 「自動產生」OTP 的預覽列：目前 OTP（三碼一組）與剩餘秒數，到期重新取碼，供與驗證器 App 比對。
 * source 回傳 null＝沒有金鑰可用。live region 只在碼或訊息變更時改字，秒數不在裡面、不會每秒念出。
 */
function otpPreview(source: () => { accountId: string | null; secret: string | null } | null): OtpPreviewView {
  const code = h("span", { class: "otp-preview-code" });
  const current = h("span", { class: "otp-preview-current" }, h("span", { class: "otp-preview-label" }, "目前 OTP"), code);
  const message = h("span", { class: "otp-preview-message" });
  const live = h("div", { class: "otp-preview-live", "aria-live": "polite", "aria-atomic": "true" }, current, message);
  const ring = h("span", { class: "otp-preview-ring", "aria-hidden": "true" });
  const left = h("span");
  // 畫面上的秒數環已表達「剩餘」，「剩」只留給輔助技術。
  const timer = h("span", { class: "otp-preview-timer" }, ring, h("span", { class: "visually-hidden" }, "剩"), left);
  const copy = h("button", { class: "ghost", type: "button", "aria-label": "複製目前 OTP" }, "複製");
  const box = h("div", { class: "otp-preview" }, live, timer, copy);

  /** 目前顯示的碼（不含空格）；沒有碼為空字串。 */
  let shown = "";
  let expiresAt = 0;
  let tick: number | undefined;
  let debounce: number | undefined;
  /** 每次取碼或停止都遞增，較早送出的回應回來時直接丟掉。 */
  let seq = 0;

  const showCode = (next: string): void => {
    shown = next;
    const text = `${next.slice(0, 3)} ${next.slice(3)}`;
    if (code.textContent !== text) code.textContent = text;
    current.hidden = false;
    message.hidden = true;
    timer.hidden = false;
    copy.hidden = false;
  };
  const showMessage = (text: string, error: boolean): void => {
    shown = "";
    if (message.textContent !== text) message.textContent = text;
    message.className = `otp-preview-message${error ? " error" : ""}`;
    current.hidden = true;
    message.hidden = false;
    timer.hidden = true;
    copy.hidden = true;
  };
  /** 畫剩餘秒數，回傳剩幾秒。只有數字變了才改 DOM。 */
  const paintLeft = (): number => {
    const s = Math.max(0, Math.ceil((expiresAt - Date.now()) / 1000));
    const text = `${s} 秒`;
    if (left.textContent !== text) {
      left.textContent = text;
      ring.style.setProperty("--otp-left", String(s / 30));
    }
    return s;
  };
  const halt = (): void => {
    seq++;
    window.clearInterval(tick);
    tick = undefined;
  };
  const fetchCode = async (): Promise<void> => {
    halt();
    const mine = seq;
    const src = source();
    if (!src) {
      showMessage("輸入金鑰後顯示目前 OTP，可與驗證器 App 比對", false);
      return;
    }
    // 以送出時的秒數起算：後端若已跨秒，到期只會提早、重取同一組碼，不會晚於真正換碼。
    const sentAt = Math.floor(Date.now() / 1000);
    try {
      const r = await api.previewOtp(src.accountId, src.secret);
      if (mine !== seq) return;
      expiresAt = (sentAt + r.secondsLeft) * 1000;
      showCode(r.code);
      paintLeft();
      // 250ms 輪詢、秒數變了才重畫：與整秒對齊，不會落後將近一秒。
      tick = window.setInterval(() => {
        if (paintLeft() === 0) void fetchCode();
      }, 250);
    } catch (e) {
      if (mine !== seq) return;
      showMessage(errorText(e), true);
    }
  };

  copy.addEventListener("click", () => {
    if (!shown) return;
    navigator.clipboard.writeText(shown).then(
      () => say("已複製目前 OTP", "ok"),
      (e: unknown) => say(`複製失敗：${errorText(e)}`, "error"),
    );
  });
  showMessage("", false);

  return {
    box,
    refresh(delay) {
      window.clearTimeout(debounce);
      debounce = undefined;
      if (delay <= 0) {
        void fetchCode();
        return;
      }
      // 輸入中：先停掉舊金鑰的倒數，停下 delay 毫秒後才取碼。
      halt();
      debounce = window.setTimeout(() => {
        debounce = undefined;
        void fetchCode();
      }, delay);
    },
    stop() {
      window.clearTimeout(debounce);
      debounce = undefined;
      halt();
    },
  };
}

/**
 * 行內重新命名：標題暫時藏起，原位換成輸入框；Enter／失焦存、Esc 取消（輸入法選字中的交給輸入法）。
 * trigger 是開始改名的控制項（「重新命名」按鈕或放著它的「⋯」選單），編輯期間藏起。
 * 名稱有變才呼叫 save（留空採 fallback）；沒變或取消時焦點回到 trigger。
 */
function renameInPlace(
  title: HTMLElement,
  trigger: HTMLElement,
  o: { value: string; fallback: string; label: string; className: string; save: (next: string) => void },
): void {
  const input = h("input", {
    class: o.className,
    "aria-label": o.label,
    placeholder: o.fallback,
    autocomplete: "off",
    spellcheck: "false",
  });
  input.value = o.value;
  let done = false;
  const finish = (commit: boolean): void => {
    if (done) return;
    done = true;
    const next = input.value.trim() || o.fallback;
    input.remove();
    title.hidden = false;
    trigger.hidden = false;
    if (!commit || next === o.value) {
      trigger.focus();
      return;
    }
    o.save(next);
  };
  input.addEventListener("keydown", (ev) => {
    if (ev.isComposing) return;
    if (ev.key === "Enter") {
      // 擋下所在表單的隱式送出（登入面板會變成登入）。
      ev.preventDefault();
      finish(true);
    } else if (ev.key === "Escape") {
      ev.preventDefault();
      finish(false);
    }
  });
  input.addEventListener("blur", () => finish(true));
  title.hidden = true;
  trigger.hidden = true;
  title.after(input);
  input.focus();
  input.select();
}

/** 切換登入表單的帳號；換帳號會丟掉前一個帳號尚未送出的輸入。 */
function selectAccount(id: string): void {
  if (selection === id) return;
  selection = id;
  draft = null;
  // 「擴充功能」分頁 Dalamud 頁與 ACT 頁的區服子分頁回到跟著目前帳號的區服。
  tabRegion = null;
  actTabRegion = null;
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

// ---------- 官方消息 ----------

/**
 * 側欄的官方消息（protocols §1.4.2、§2.7）：跟登入面板目前的區服（新增帳號時跟表單選的區服），國際服再依「進階 › 語言」
 * 取該語言的公告。各服（國際服各語言）各自快取：第一次用到才抓，之後換帳號不重抓、也不定時重抓；取不到時可點擊重試，
 * 只重抓那一份，原因放在重試按鈕的 title。
 */
type NewsState = { kind: "loading" } | { kind: "error"; reason: string } | { kind: "loaded"; items: NewsItem[] };
/** 要顯示的那一份消息；key 是快取鍵（國際服含語言）。 */
interface NewsSource {
  region: Region;
  language: GlobalLanguage;
  key: string;
}
const newsCache = new Map<string, NewsState>();
/** accountPane() 新建的面板；抓取結果回來，或區服、語言改了時就地更新，不重繪整個登入頁。 */
let newsPanel: { root: HTMLElement; title: HTMLElement; body: HTMLElement } | null = null;

function newsSource(d: LoginDraft): NewsSource {
  return { region: d.region, language: d.language, key: d.region === "global" ? `global-${d.language}` : "tw" };
}

function loadNews(src: NewsSource): void {
  newsCache.set(src.key, { kind: "loading" });
  paintNews();
  const fetching = src.region === "global" ? api.getGlobalNews(src.language) : api.getTwNews();
  void fetching.then(
    (items) => {
      newsCache.set(src.key, { kind: "loaded", items });
      paintNews();
    },
    (e: unknown) => {
      newsCache.set(src.key, { kind: "error", reason: errorText(e) });
      paintNews();
    },
  );
}

/** 開消息不走 run()：不鎖按鈕、不重繪，失敗只顯示訊息（同 openFolder）。 */
function openNews(region: Region, url: string): void {
  api.openNews(region, url).catch((e: unknown) => say(errorText(e), "error"));
}

/** 依登入面板目前的區服與語言重畫消息面板；那一份還沒抓過就開始抓。 */
function paintNews(): void {
  if (!newsPanel || !draft) return;
  const src = newsSource(draft);
  const s = newsCache.get(src.key);
  if (!s) {
    loadNews(src);
    return;
  }
  const { root, title, body } = newsPanel;
  root.className = `news-group ${src.region}`;
  title.textContent = `${regionLabel(src.region)}消息`;
  switch (s.kind) {
    case "loading":
      body.replaceChildren(h("p", { class: "news-note" }, "載入中…"));
      return;
    case "error": {
      const retry = h("button", { type: "button", class: "news-note news-retry", title: s.reason }, "無法取得消息，點此重試");
      retry.addEventListener("click", () => loadNews(src));
      body.replaceChildren(retry);
      return;
    }
    case "loaded":
      body.replaceChildren(
        s.items.length
          ? h("ul", { class: "news-list" }, ...s.items.map((n) => newsItem(src.region, n)))
          : h("p", { class: "news-note" }, "目前沒有消息"),
      );
  }
}

/** 一則消息：標題（最多兩行，超出省略，完整標題在 title）與日期各一行；點擊以系統瀏覽器開啟。 */
function newsItem(region: Region, n: NewsItem): HTMLElement {
  const b = h(
    "button",
    { type: "button", class: "news-item", title: n.title },
    h("span", { class: "news-item-title" }, n.title),
    h("span", { class: "news-item-date" }, n.date),
  );
  b.addEventListener("click", () => openNews(region, n.url));
  return h("li", {}, b);
}

/** 側欄帳號清單下方的消息面板：區服色底與細條，標題（圖示＋「國際服消息」／「繁中服消息」）用區服色。 */
function newsGroup(): HTMLElement {
  const title = h("span");
  const body = h("div", { class: "news-body" });
  const root = h(
    "section",
    { class: "news-group", "aria-labelledby": "news-title" },
    h("h3", { class: "news-title", id: "news-title" }, h("span", { class: "news-icon", "aria-hidden": "true" }), title),
    body,
  );
  newsPanel = { root, title, body };
  paintNews();
  return root;
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

function renderLogin(): void {
  // 重繪（含換帳號）前先停掉上一個面板的 OTP 預覽，不留下多組計時器。
  stopOtpPreview();
  const a = selectedAccount();
  if (!draft || draft.accountId !== (a?.id ?? null)) draft = draftFor(a);
  const d = draft;
  const error = loginError;
  loginError = null;
  const errorFor = (f: LoginField): string | null => (error?.field === f ? error.text : null);
  const bound = a ? config.profiles.find((p) => p.id === a.profileId) : undefined;

  // 內容區頂部的頁首：選中的帳號（新增模式為「新增帳號」）與同一行的區服 badge（標題的屬性）一組；
  // 已存帳號的右側是「⋯」選單：重新命名｜檢查遊戲更新或遊戲檔完整性（依區服）、開啟遊戲設定資料夾｜刪除帳號。新增模式沒有選單。
  const title = h("h2", { class: "login-title", id: "login-title" }, a ? accountLabel(a) : "新增帳號");
  const regionBadge = h("span");
  const identity = h("div", { class: "login-identity" }, title, regionBadge);
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
        if (d.region === "global") reloginPending = false;
        const started = `已啟動 ${label}（pid ${r.pid}）${loaded}`;
        // 遊戲已啟動，但 ACT 沒開起來或 ACT 連動沒更新：附上原因，不用錯誤樣式。
        const notes = [r.actWarning, r.actTargetWarning].filter((w): w is string => w !== null);
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
function focusLoginStart(): void {
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

// ---------- Dalamud 設定檔 ----------

/**
 * 一個可套用的 Dalamud 設定：舊啟動器資料（可能已有沿用它的設定檔）或本程式的設定檔。
 * 登入面板的「Dalamud 設定檔」下拉與「擴充功能 › Dalamud」的設定檔卡片共用同一份清單。
 */
interface DalamudChoice {
  /** 下拉 option 的 value：設定檔 id；還沒有設定檔的舊資料為 `detected:<path>`（設定檔 id 是 uuid，不會撞）。 */
  value: string;
  target: BindTarget;
  name: string;
  path: string;
  pluginCount: number;
  region: Region;
  /** null＝偵測到、但還沒有帳號選過的舊資料。 */
  profile: Profile | null;
  /** 對應的舊啟動器資料；null＝本程式設定檔（含沿用的資料夾已偵測不到的）。 */
  legacy: DetectedDalamudData | null;
}

function profileChoice(p: Profile, legacy: DetectedDalamudData | null): DalamudChoice {
  return {
    value: p.id,
    target: { kind: "profile", id: p.id },
    name: p.name,
    path: p.dataDir ?? `${config.dataDir}\\profiles\\${p.id}`,
    pluginCount: config.profileInfo.find((i) => i.id === p.id)?.pluginCount ?? legacy?.pluginCount ?? 0,
    region: channelRegion(p.channel),
    profile: p,
    legacy,
  };
}

/** 某區服可套用的 Dalamud 設定：舊啟動器資料（已有設定檔沿用的就代表那個設定檔，不重複列）與其餘設定檔。 */
function dalamudChoices(region: Region): { legacy: DalamudChoice[]; own: DalamudChoice[] } {
  const legacy = (detected ?? [])
    .filter((d) => d.region === region)
    .map((d): DalamudChoice => {
      // 與後端 check_data_dir 一樣以路徑字串比對。
      const p = config.profiles.find((x) => x.dataDir === d.path);
      if (p) return profileChoice(p, d);
      return {
        value: `detected:${d.path}`,
        target: { kind: "detected", path: d.path },
        name: d.label,
        path: d.path,
        pluginCount: d.pluginCount,
        region: d.region,
        profile: null,
        legacy: d,
      };
    });
  const own = config.profiles
    .filter((p) => channelRegion(p.channel) === region && !legacy.some((c) => c.profile?.id === p.id))
    .map((p) => profileChoice(p, null));
  return { legacy, own };
}

function choiceText(c: DalamudChoice): string {
  const stale = c.profile?.dataDir && !c.legacy ? "，目前找不到資料夾" : "";
  return `${c.name}（${c.pluginCount} 個插件${stale}）`;
}

/**
 * 填入帳號的「Dalamud 設定檔」下拉：只列該區服的選項，並選中目前綁定的設定檔 current。
 * 回傳 option value → 選項；"" 為不載入 Dalamud。
 */
function fillDalamudSelect(select: HTMLSelectElement, region: Region, current: string | null): Map<string, DalamudChoice> {
  const { legacy, own } = dalamudChoices(region);
  const choices = new Map([...legacy, ...own].map((c): [string, DalamudChoice] => [c.value, c]));
  select.replaceChildren(option("", "不載入 Dalamud", !current));
  if (current && !choices.has(current)) {
    // 綁到不存在或不同區服的設定檔（例如手動改過設定）：照實顯示，選別的即可修正。
    const invalid = option(current, "（設定檔不存在或區服不符）", true);
    invalid.disabled = true;
    select.append(invalid);
  }
  const groups: [string, DalamudChoice[]][] = [
    ["舊啟動器資料", legacy],
    ["本程式設定檔", own],
  ];
  for (const [label, list] of groups) {
    if (!list.length) continue;
    select.append(h("optgroup", { label }, ...list.map((c) => option(c.value, choiceText(c), c.value === current))));
  }
  return choices;
}

function bindTarget(choices: Map<string, DalamudChoice>, value: string): BindTarget {
  return choices.get(value)?.target ?? { kind: "none" };
}

function ensureTwVersions(): Promise<string[]> {
  twVersionsLoad ??= api.listYanmucorpReleases().then(
    (list) => {
      const tags = list.map((r) => r.tag);
      twVersions = tags;
      twVersionsError = null;
      return tags;
    },
    (e: unknown) => {
      // 失敗不快取 promise，之後手動重試才會再抓。
      twVersionsLoad = null;
      twVersionsError = errorText(e);
      throw e;
    },
  );
  return twVersionsLoad;
}

/** 某區服已下載的 Dalamud，新到舊（國際服為點分數字、台服為 yy-mm-dd-nn，都依數字排序）。 */
function installedOf(region: Region): InstalledVersion[] {
  return config.installed
    .filter((v) => isRegionInstall(v, region))
    .sort((x, y) => y.version.localeCompare(x.version, undefined, { numeric: true }));
}

/** 台服 release tag（yy-mm-dd-nn：發布日期＋當日序號）→「20yy-mm-dd 版」，同日第 n 版（n>1）→「20yy-mm-dd 第 n 版」；格式不符原樣顯示。 */
function twReleaseLabel(tag: string): string {
  const m = /^(\d{2})-(\d{2})-(\d{2})-(\d{2})$/.exec(tag);
  if (!m) return tag;
  const n = Number(m[4]);
  return `20${m[1]}-${m[2]}-${m[3]} ${n > 1 ? `第 ${n} 版` : "版"}`;
}

/**
 * 已安裝 Dalamud 的顯示：dirVersion＝安裝目錄名，dalamudVersion＝其中 `Dalamud.dll` 的檔案版本（讀不到為 null）。
 * 國際服「Dalamud 15.0.3.6」；台服「Dalamud 13.0.0.16（2026-10-03 版）」，沒有檔案版本時只顯示日期標籤。
 */
function dalamudVersionLabel(region: Region, dirVersion: string, dalamudVersion: string | null): string {
  // 國際服的目錄名就是 AssemblyVersion，讀不到檔案版本時拿來頂替。
  if (region === "global") return `Dalamud ${dalamudVersion ?? dirVersion}`;
  const date = twReleaseLabel(dirVersion);
  return dalamudVersion ? `Dalamud ${dalamudVersion}（${date}）` : date;
}

function findInstalled(region: Region, dirVersion: string): InstalledVersion | undefined {
  return config.installed.find((v) => isRegionInstall(v, region) && v.version === dirVersion);
}

/** 台服 tag 的顯示：已下載的附上 Dalamud 版本，否則只有日期標籤。 */
function twTagLabel(tag: string): string {
  const v = findInstalled("tw", tag);
  return v ? dalamudVersionLabel("tw", tag, v.dalamudVersion) : twReleaseLabel(tag);
}

/**
 * 安裝目錄 dirVersion 的 `Dalamud.dll` 檔案版本，給下載／啟動完成的訊息用。
 * 清單裡還沒有（例如剛下載好）就重讀一次；讀不到為 null，訊息退回只顯示目錄版本。
 */
async function installedDalamudVersion(region: Region, dirVersion: string): Promise<string | null> {
  if (!findInstalled(region, dirVersion)) {
    // 讀取失敗只少了版本號；run() 結束重繪時會再讀一次並在訊息列顯示原因。
    config.installed = await api.listInstalledDalamud().catch(() => config.installed);
  }
  return findInstalled(region, dirVersion)?.dalamudVersion ?? null;
}

/**
 * 台服版本下拉：「最新版」＋可下載的版本（已下載的加註）；current＝選中的版本，""＝最新版。
 * 版本清單還沒抓到時先只列目前的選擇，抓到後就地填入；抓不到時只列已下載的版本，原因與「重試」放在 error。
 */
function twVersionSelect(id: string, current: string): { select: HTMLSelectElement; error: HTMLElement } {
  const select = h("select", { id });
  const error = h("p", { class: "field-error" });
  error.hidden = true;
  const fill = (tags: readonly string[]): void => {
    const have = new Map(installedOf("tw").map((v): [string, InstalledVersion] => [v.version, v]));
    const list = [...tags];
    // 選中的版本可能已不在清單上；照實列出，免得下拉看起來像「最新版」。
    if (current && !list.includes(current)) list.unshift(current);
    select.replaceChildren(
      option("", "最新版", current === ""),
      ...list.map((t) => {
        const v = have.get(t);
        return option(t, v ? `${dalamudVersionLabel("tw", t, v.dalamudVersion)}・已下載` : twReleaseLabel(t), t === current);
      }),
    );
  };
  const fail = (text: string): void => {
    fill(installedOf("tw").map((v) => v.version));
    const retry = h("button", { class: "ghost", type: "button" }, "重試");
    retry.addEventListener("click", () => {
      twVersionsError = null;
      void render();
    });
    error.replaceChildren(`無法讀取版本清單：${text} `, retry);
    error.hidden = false;
  };
  if (twVersions) {
    fill(twVersions);
  } else if (twVersionsError !== null) {
    fail(twVersionsError);
  } else {
    // 只有一個選項，讀取期間無法改選；回來後就地填入或換成失敗提示。
    select.append(option(current, "讀取版本清單…", true));
    void ensureTwVersions().then(fill, (e: unknown) => fail(errorText(e)));
  }
  return { select, error };
}

function deleteProfile(p: Profile): void {
  const bound = config.accounts.filter((a) => a.profileId === p.id);
  if (bound.length) {
    say(`「${p.name}」仍套用於 ${bound.map(accountLabel).join("、")}，請先到「登入」分頁把這些帳號的 Dalamud 設定檔改掉再刪除。`, "error");
    return;
  }
  const kept = p.dataDir
    ? `舊啟動器的資料夾（${p.dataDir}）與其中的插件、設定都不會動。`
    : "資料夾內的插件與設定會保留。";
  if (confirm(`刪除 Dalamud 設定檔「${p.name}」？\n只移除本程式裡的這個設定項；${kept}`)) {
    void run("刪除 Dalamud 設定檔", () => api.deleteProfile(p.id));
  }
}

/**
 * 進行中的「複製」：進度畫在來源設定檔的卡片裡。run() 期間按鈕全鎖（進度的「取消」除外），同時至多一筆；
 * 進度狀態放這裡而非 DOM，因為 render() 會重建面板。
 */
let duplicating: { id: string; progress: ProgressState | null } | null = null;
/** profileBlock() 時重建：複製中的進度元素。 */
let duplicateProgress: ProgressView | null = null;

/**
 * 複製設定檔：設定與插件本體（不含插件資料與快取、log），一律複製進本程式的資料夾；完成後新設定檔出現在同一區服的清單。
 */
function duplicateProfile(p: Profile): void {
  duplicating = { id: p.id, progress: null };
  renderDalamud();
  duplicateProgress?.cancel.focus();
  let created = p.id;
  void run(`複製 Dalamud 設定檔「${p.name}」`, async () => {
    try {
      const r = await api.duplicateProfile(p.id, null, (e) => {
        if (!duplicating) return;
        duplicating.progress = progressState(e);
        if (duplicateProgress) paintProgress(duplicateProgress, duplicating.progress);
      });
      created = r.id;
      const links = r.skippedLinks ? `；略過 ${r.skippedLinks} 個連結（symlink／junction 不複製）` : "";
      return `已複製為「${r.name}」。插件資料與快取不複製，插件首次執行時會重建${links}`;
    } finally {
      // 在 run() 重繪之前清掉，重建的卡片就回到一般狀態。
      duplicating = null;
    }
  }).then(() => document.getElementById(`profile-copy-${created}`)?.focus());
}

/** 「擴充功能 › Dalamud」正在編輯的設定檔卡片；同時只有一張。欄位值存這裡，重繪沿用。 */
interface ProfileDraft {
  region: Region;
  /** 編輯中的設定檔 id；"" 為新設定檔（插在清單最後）。 */
  id: string;
  name: string;
  /** 台服版本；""＝最新版。 */
  tag: string;
  delay: string;
  advancedOpen: boolean;
}

/**
 * 新設定檔的注入延遲預設值（毫秒）；須與後端 `Region::default_injection_delay_ms` 同步。
 * 繁中服是 attach 注入，5000 比 0（改走延遲初始化 10000）約快 3 秒載入插件；國際服 launch 注入，0＝不延遲。
 */
const DEFAULT_INJECTION_DELAY_MS: Record<Region, number> = { global: 0, tw: 5000 };

let profileDraft: ProfileDraft | null = null;

/** 把卡片就地展開成編輯表單（p＝null 為新設定檔）；另一張編輯中的卡片直接收起，未存的變更捨棄。 */
function editProfile(region: Region, p: Profile | null): void {
  const version = p ? channelVersion(p.channel) : null;
  profileDraft = {
    region,
    id: p?.id ?? "",
    name: p?.name ?? "",
    tag: version?.kind === "pinned" ? version.tag : "",
    delay: String(p?.injectionDelayMs ?? DEFAULT_INJECTION_DELAY_MS[region]),
    advancedOpen: false,
  };
  renderDalamud();
  const name = document.getElementById("profile-name") as HTMLInputElement | null;
  name?.closest(".card")?.scrollIntoView({ block: "nearest" });
  name?.focus();
  name?.select();
}

/** 編輯中的卡片本身：名稱、（台服）版本、折疊的進階；儲存或取消後變回一般卡片。 */
function profileEditor(d: ProfileDraft, p: Profile | null): HTMLFormElement {
  const name = h("input", { id: "profile-name", autocomplete: "off", spellcheck: "false" });
  name.value = d.name;
  name.addEventListener("input", () => (d.name = name.value));
  const fields = [loginField("名稱", name, null)];
  // 只有台服能選版本；國際服一律最新版。區服跟著所在子分頁，不讓使用者換。
  if (d.region === "tw") {
    const { select, error } = twVersionSelect("profile-version", d.tag);
    select.addEventListener("change", () => (d.tag = select.value));
    const versionField = loginField("版本", select, null);
    versionField.append(error);
    fields.push(versionField);
  }
  const delay = h("input", { id: "profile-delay", type: "number", min: "0", step: "500" });
  delay.value = d.delay;
  delay.addEventListener("input", () => (d.delay = delay.value));
  const advanced = h(
    "details",
    { class: "editor-advanced" },
    h("summary", {}, "進階"),
    h("div", { class: "editor-advanced-body" }, loginField("啟動後延遲載入（毫秒）", delay, null)),
  );
  advanced.open = d.advancedOpen;
  advanced.addEventListener("toggle", () => (d.advancedOpen = advanced.open));

  const save = h("button", { class: "primary", type: "submit" }, "儲存");
  const cancel = h("button", { class: "secondary", type: "button" }, "取消");
  const form = h(
    "form",
    { class: "card profile-editor", "aria-label": p ? `編輯「${p.name}」` : "新設定檔" },
    h("div", { class: "card-head" }, h("span", { class: "title" }, p ? p.name : "新設定檔")),
    ...fields,
    advanced,
    h("div", { class: "editor-actions" }, save, cancel),
  );
  // 焦點回到開啟編輯的地方：卡片的「更多」或「新增空白設定檔」。
  const close = (): void => {
    profileDraft = null;
    renderDalamud();
    document.getElementById(p ? `profile-more-${p.id}` : "profiles-add")?.focus();
  };
  cancel.addEventListener("click", close);
  form.addEventListener("keydown", (ev) => {
    if (ev.key !== "Escape" || ev.isComposing) return;
    ev.preventDefault();
    close();
  });
  form.addEventListener("submit", (ev) => {
    ev.preventDefault();
    let id = d.id;
    void run("儲存 Dalamud 設定檔", async () => {
      id = await api.saveProfile({
        id: d.id,
        name: d.name.trim(),
        channel: regionChannel(d.region, d.tag ? { kind: "pinned", tag: d.tag } : { kind: "latest" }),
        injectionDelayMs: Math.max(0, Number(d.delay) || 0),
        dataDir: p?.dataDir ?? null,
        shareConfirmed: p?.shareConfirmed ?? false,
      });
      // 存好才收起；失敗時留在編輯中，草稿沿用。
      if (profileDraft === d) profileDraft = null;
    }).then(() => document.getElementById(profileDraft === d ? "profile-name" : `profile-more-${id}`)?.focus());
  });
  return form;
}

/** 各區服的舊啟動器。 */
const LAUNCHER: Record<Region, string> = { global: "XIVLauncher", tw: "TC 啟動器" };

/** 設定的來源，一句話；設定檔卡片用。 */
function choiceSource(c: DalamudChoice): string {
  if (c.legacy) return `來自 ${LAUNCHER[c.region]}`;
  return c.profile?.dataDir ? `來自 ${LAUNCHER[c.region]}（目前找不到資料夾）` : "本程式新增";
}

interface MenuItem {
  label: string;
  action: () => void;
  /** 沿用原本按鈕的 id（動作結束後的焦點等據此找到它）。 */
  id?: string;
  danger?: boolean;
  disabled?: boolean;
  /** 滑鼠停留說明，例如停用的原因。 */
  title?: string;
}

/**
 * 「更多」選單：原生 details；選一項、按 Esc 或點選單外即收起。groups 是分組的項目，組與組之間畫分隔線（空組略過）。
 * icon＝summary 是低調的「⋯」圖示按鈕（「更多」只給輔助技術與滑鼠停留說明）；否則是寫著「更多」的次要按鈕。
 */
function moreMenu(id: string, groups: MenuItem[][], icon = false): HTMLDetailsElement {
  const summary = icon
    ? h("summary", { id, title: "更多" }, moreIcon(), h("span", { class: "visually-hidden" }, "更多"))
    : h("summary", { id }, "更多");
  const list = h("div", { class: "menu-list" });
  const menu = h("details", { class: icon ? "menu menu-icon" : "menu" }, summary, list);
  for (const group of groups.filter((g) => g.length > 0)) {
    if (list.childElementCount > 0) list.append(h("div", { class: "menu-sep", role: "separator" }));
    for (const { id: itemId, label, action, danger, disabled, title } of group) {
      const item = h("button", { id: itemId, type: "button", class: danger ? "menu-item menu-danger" : "menu-item", title }, label);
      item.disabled = disabled === true;
      item.addEventListener("click", () => {
        menu.open = false;
        action();
      });
      list.append(item);
    }
  }
  menu.addEventListener("keydown", (ev) => {
    if (ev.key !== "Escape" || !menu.open) return;
    menu.open = false;
    summary.focus();
  });
  return menu;
}

// 浮動的 details（「更多」選單、標題列的更新說明）：點在外面就收起。
document.addEventListener("click", (ev) => {
  document.querySelectorAll<HTMLDetailsElement>("details.menu[open], details.app-update-notes[open]").forEach((m) => {
    if (!m.contains(ev.target as Node)) m.open = false;
  });
});

/** 編輯偵測到的舊資料：先取得（必要時新增）對應的設定檔並重繪，再就地展開編輯。 */
async function editDetected(c: DalamudChoice): Promise<void> {
  try {
    const id = await api.importDetectedProfile(c.path, null);
    await render();
    const p = config.profiles.find((x) => x.id === id);
    if (p) editProfile(c.region, p);
  } catch (e) {
    say(errorText(e), "error");
  }
}

/**
 * 總覽卡片：名稱；來源・插件數；套用的帳號（沒有就不顯示）。
 * 偵測到、還沒有設定檔的舊資料外觀與操作相同：重新命名、編輯時由後端取得（必要時新增）對應的設定檔。
 */
function choiceCard(c: DalamudChoice): HTMLElement {
  const p = c.profile;
  // 元素 id 的鍵：設定檔 id；舊資料用編碼後的路徑（不含空白）。
  const key = p?.id ?? `detected-${encodeURIComponent(c.path)}`;
  const meta = [h("span", {}, choiceSource(c)), h("span", {}, `${c.pluginCount} 個插件`)];
  const version = p ? channelVersion(p.channel) : null;
  if (version?.kind === "pinned") meta.push(h("span", {}, `固定 ${twTagLabel(version.tag)}`));
  const title = h("span", { class: "title" }, c.name);
  const head = h("div", { class: "card-head" }, title);
  const main = h("div", { class: "card-main" }, head, h("div", { class: "meta" }, ...meta));
  const card = h("div", { class: "card" }, main);
  const rename = h("button", { id: `profile-rename-${key}`, class: "ghost", type: "button" }, "重新命名");
  rename.addEventListener("click", () =>
    // 留空＝不改名。
    renameInPlace(title, rename, {
      value: c.name,
      fallback: c.name,
      label: "設定檔名稱",
      className: "card-title-input",
      save: (next) => {
        let id = key;
        void run("重新命名 Dalamud 設定檔", async () => {
          id = p ? await api.saveProfile({ ...p, name: next }) : await api.importDetectedProfile(c.path, next);
          return `已改名為「${next}」`;
        }).then(() => document.getElementById(`profile-rename-${id}`)?.focus());
      },
    }),
  );
  head.append(rename, ...(p && config.profilesInUse.includes(p.id) ? [h("span", { class: "badge running" }, "使用中")] : []));
  const bound = p ? config.accounts.filter((a) => a.profileId === p.id) : [];
  if (bound.length) {
    main.append(
      h(
        "div",
        { class: "chips" },
        h("span", { class: "chips-label" }, "套用於"),
        ...bound.map((a) => h("span", { class: `chip ${c.region}` }, accountLabel(a))),
      ),
    );
  }
  const openDir = h("button", { class: "secondary", type: "button", title: c.path }, "開啟資料夾");
  openDir.addEventListener("click", () => openFolder(p ? { kind: "profile", id: p.id } : { kind: "detected", path: c.path }));
  const running = p !== null && config.profilesInUse.includes(p.id);
  const actions: HTMLElement[] = [openDir];
  if (p) {
    // 雙開同服帳號要靠複本，放在卡片上而非「更多」裡。遊戲執行中衛月會持續寫入設定，後端也會拒絕。
    const copy = h(
      "button",
      {
        id: `profile-copy-${p.id}`,
        class: "secondary",
        type: "button",
        title: running ? "遊戲執行中，請先關閉遊戲再複製" : "複製設定與插件；插件資料與快取不複製，插件首次執行時會重建",
      },
      "複製",
    );
    copy.disabled = running;
    copy.addEventListener("click", () => duplicateProfile(p));
    actions.push(copy);
  }
  const items: MenuItem[] = p
    ? [
        { label: "開啟 log", action: () => openFolder({ kind: "profileLogs", id: p.id }) },
        { label: "編輯", action: () => editProfile(c.region, p) },
        // 只移除本程式的設定項；沿用的舊資料夾不會被刪。
        { label: "刪除", action: () => deleteProfile(p), danger: true },
      ]
    : [
        { label: "開啟 log", action: () => undefined, disabled: true, title: "尚未使用過，沒有 log" },
        { label: "編輯", action: () => void editDetected(c) },
      ];
  card.append(h("div", { class: "card-actions" }, ...actions, moreMenu(`profile-more-${key}`, [items])));
  if (p && duplicating?.id === p.id) {
    duplicateProgress = progressView(`複製「${p.name}」進度`, false);
    paintProgress(duplicateProgress, duplicating.progress ?? { text: "準備複製…", done: 0, total: null, cancellable: false });
    main.append(duplicateProgress.box);
  }
  return card;
}

// ---------- 擴充功能分頁 ----------

interface SubtabSpec<T extends string> {
  /** tablist 的 aria-label。 */
  label: string;
  tabs: readonly T[];
  current: T;
  /** 分頁按鈕的 id；select 重繪後以它找回焦點。 */
  id: (t: T) => string;
  /** 分頁控制的 tabpanel id。 */
  controls: (t: T) => string;
  className: (t: T) => string;
  content: (t: T) => Child[];
  select: (t: T) => void;
}

/** 子分頁的 tablist（WAI-ARIA tabs）：「擴充功能」的 Dalamud／ACT 與 Dalamud 頁的區服共用。 */
function subtabs<T extends string>(s: SubtabSpec<T>): HTMLElement {
  const tabs = s.tabs.map((t) => {
    const selected = t === s.current;
    const tab = h(
      "button",
      {
        type: "button",
        role: "tab",
        id: s.id(t),
        class: s.className(t),
        "aria-selected": String(selected),
        "aria-controls": s.controls(t),
        tabindex: selected ? "0" : "-1",
      },
      ...s.content(t),
    );
    tab.addEventListener("click", () => s.select(t));
    return tab;
  });
  const tablist = h("div", { class: "subtabs", role: "tablist", "aria-label": s.label }, ...tabs);
  // 左右方向鍵切換子分頁（WAI-ARIA tabs 慣例；只有選中的分頁在 Tab 順序裡）。
  tablist.addEventListener("keydown", (ev) => {
    const step = ev.key === "ArrowRight" ? 1 : ev.key === "ArrowLeft" ? -1 : 0;
    if (!step) return;
    ev.preventDefault();
    s.select(s.tabs[(s.tabs.indexOf(s.current) + step + s.tabs.length) % s.tabs.length]);
  });
  return tablist;
}

type ExtTab = "dalamud" | "act";
const EXT_TABS: readonly ExtTab[] = ["dalamud", "act"];
const EXT_TAB_TEXT: Record<ExtTab, string> = { dalamud: "Dalamud", act: "ACT" };
/** 「擴充功能」目前的子分頁；只存記憶體。 */
let extTab: ExtTab = "dalamud";

/**
 * 子分頁的面板：元素只建一次，renderDalamud()／renderAct() 各自重建內容。
 * id 用 ext- 前綴，不與 Dalamud 頁內的區服子分頁（dalamud-tab-*、dalamud-panel）重複。
 */
const extPanels: Record<ExtTab, HTMLElement> = {
  dalamud: h("div", { id: "ext-panel-dalamud", role: "tabpanel", "aria-labelledby": "ext-tab-dalamud" }),
  act: h("div", { id: "ext-panel-act", role: "tabpanel", "aria-labelledby": "ext-tab-act" }),
};

function renderExtensions(): void {
  for (const t of EXT_TABS) extPanels[t].hidden = t !== extTab;
  const tablist = subtabs({
    label: "擴充功能",
    tabs: EXT_TABS,
    current: extTab,
    id: (t) => `ext-tab-${t}`,
    controls: (t) => `ext-panel-${t}`,
    className: () => "subtab",
    content: (t) => [EXT_TAB_TEXT[t]],
    select: (t) => {
      extTab = t;
      renderExtensions();
      document.getElementById(`ext-tab-${t}`)?.focus();
    },
  });
  // Dalamud／ACT 兩個子分頁共用的風險提醒；連結切到「設定 › 免責與安全」。
  const details = h("button", { class: "link", type: "button" }, "詳見「設定 → 免責與安全」");
  details.addEventListener("click", showSafety);
  const notice = h("p", { class: "hint ext-risk-note" }, "這些是第三方工具，可能違反遊戲條款，風險自負。", details);
  section("extensions").replaceChildren(notice, tablist, ...EXT_TABS.map((t) => extPanels[t]));
  renderDalamud();
  renderAct();
}

// ---------- Dalamud 分頁 ----------

/** 子分頁的區服；null＝跟登入面板目前帳號的區服（登入面板換帳號時重設）。 */
let tabRegion: Region | null = null;

function currentTabRegion(): Region {
  return tabRegion ?? draft?.region ?? "global";
}

/**
 * 進行中的「下載／更新」：進度依 install:<region> 事件畫在該區服的子分頁。
 * run() 期間按鈕全鎖，同時至多一筆。進度狀態放這裡而非 DOM，因為 render() 會重建面板。
 */
let installing: { region: Region; progress: ProgressState | null } | null = null;
/** renderDalamud() 時重建：下載中的進度元素。 */
let installProgress: ProgressView | null = null;
/** 台服下拉選的版本；""＝最新版，null＝還沒選過（預設已下載的最新版，沒有就最新版）。重繪沿用。 */
let twInstallChoice: string | null = null;

function installRegion(region: Region, version: YanmucorpVersion | null): void {
  installing = { region, progress: null };
  // 先畫出「下載中…」、進度與「取消」，run() 再鎖住其餘按鈕。
  renderDalamud();
  installProgress?.cancel.focus();
  void run(`下載${regionLabel(region)} Dalamud`, async () => {
    try {
      const dir = await api.installDalamud(region, version, "enforce");
      return `已下載${regionLabel(region)} ${dalamudVersionLabel(region, dir, await installedDalamudVersion(region, dir))}`;
    } finally {
      // 在 run() 重繪之前清掉，重建的面板就回到一般狀態。
      installing = null;
    }
  }).then(() => document.getElementById("dalamud-install")?.focus());
}

function renderDalamud(): void {
  // 編輯中的設定檔已被刪除（例如從「更多」刪掉）：收起編輯。
  const d = profileDraft;
  if (d?.id && !config.profiles.some((p) => p.id === d.id)) profileDraft = null;
  const lists: Record<Region, DalamudChoice[]> = { global: [], tw: [] };
  for (const r of REGIONS) {
    const { legacy, own } = dalamudChoices(r);
    lists[r] = [...legacy, ...own];
  }
  const current = currentTabRegion();
  extPanels.dalamud.replaceChildren(
    h(
      "p",
      { class: "hint" },
      "選好要用的版本並下載；設定檔決定插件與設定，在「登入」頁為每個帳號選擇。套用舊啟動器資料時，請勿同時用舊啟動器開同一份設定。",
    ),
    ...(detected === null ? [h("p", { class: "hint" }, "無法偵測舊啟動器資料，原因見下方訊息列。")] : []),
    regionTabs({ global: lists.global.length, tw: lists.tw.length }),
    h(
      "div",
      { id: "dalamud-panel", role: "tabpanel", "aria-labelledby": `dalamud-tab-${current}` },
      versionBlock(current),
      profileBlock(current, lists[current]),
    ),
  );
}

/** 區服子分頁的 tablist；數字為各區服的設定檔數。 */
function regionTabs(counts: Record<Region, number>): HTMLElement {
  return subtabs({
    label: "區服",
    tabs: REGIONS,
    current: currentTabRegion(),
    id: (r) => `dalamud-tab-${r}`,
    controls: () => "dalamud-panel",
    className: (r) => `subtab ${r}`,
    content: (r) => [regionLabel(r), h("span", { class: "subtab-count", title: `${counts[r]} 個設定檔` }, String(counts[r]))],
    select: (r) => {
      tabRegion = r;
      renderDalamud();
      document.getElementById(`dalamud-tab-${r}`)?.focus();
    },
  });
}

/**
 * 「版本」區（小標題＋細分隔線，扁平無框）：一列「版本（台服下拉／國際服文字）＋主按鈕＋更多」，下方為下載進度。
 * 主按鈕依選到的版本：未下載「下載」；已下載的最新版（國際服一律）「檢查更新」；台服固定版本已下載則停用並顯示「已下載」。
 */
function versionBlock(region: Region): HTMLElement {
  const list = installedOf(region);
  const job = installing?.region === region ? installing : null;
  const bar = h("div", { class: "install-bar" });
  const block = h(
    "section",
    { class: "dalamud-block", "aria-labelledby": "dalamud-version-title" },
    h("h3", { class: "subsection-title", id: "dalamud-version-title" }, "版本"),
    bar,
  );
  /** 台服下拉選的版本；""＝最新版。國際服固定最新版。 */
  let tag = "";
  /** 選到／顯示的版本若已下載，即「更多」裡「刪除此版本」的對象。 */
  let target: InstalledVersion | undefined;
  if (region === "tw") {
    tag = twInstallChoice ?? list[0]?.version ?? "";
    target = list.find((v) => v.version === tag);
    const { select, error } = twVersionSelect("dalamud-tw-version", tag);
    select.setAttribute("aria-label", `${regionLabel("tw")} Dalamud 版本`);
    select.disabled = job !== null;
    // 主按鈕與「更多」都依選擇而變：整個分頁重繪後焦點留在下拉。
    select.addEventListener("change", () => {
      twInstallChoice = select.value;
      renderDalamud();
      document.getElementById("dalamud-tw-version")?.focus();
    });
    bar.append(select);
    block.append(error);
  } else {
    target = list[0];
    const status = target ? dalamudVersionLabel(region, target.version, target.dalamudVersion) : "尚未下載";
    bar.append(h("span", { class: "install-status" }, status));
  }

  const latest = region === "global" || tag === "";
  const downloaded = latest ? list.length > 0 : target !== undefined;
  // 主要按鈕只留給「下載」：已下載時「檢查更新」是例行動作，用次要樣式。
  const button = h(
    "button",
    { id: "dalamud-install", class: downloaded ? "secondary" : "primary", type: "button" },
    job ? "下載中…" : !downloaded ? "下載" : latest ? "檢查更新" : "已下載",
  );
  // 下載前要依遊戲版本檢查相容性，沒有遊戲路徑就無法下載。
  const noGame = config.games[region] === null;
  button.disabled = job !== null || noGame || (downloaded && !latest);
  button.addEventListener("click", () =>
    installRegion(region, region === "global" ? null : tag ? { kind: "pinned", tag } : { kind: "latest" }),
  );

  const keep = target ?? list[0];
  const others = list.filter((v) => v !== keep);
  const items: MenuItem[] = [
    { label: "刪除此版本", action: () => keep && removeVersions(region, [keep]), danger: true, disabled: !target },
  ];
  if (others.length) items.push({ label: "刪除其他舊版本", action: () => removeVersions(region, others), danger: true });
  bar.append(button, moreMenu("dalamud-version-more", [items]));
  if (noGame) block.append(h("p", { class: "hint" }, `尚未設定${regionLabel(region)}遊戲路徑，請先到「設定」分頁填寫。`));

  installProgress = null;
  if (job) {
    installProgress = progressView(`${regionLabel(region)} Dalamud 下載進度`, false);
    paintProgress(installProgress, job.progress ?? { text: "準備下載…", done: 0, total: null, cancellable: false });
    block.append(installProgress.box);
  }
  return block;
}

/** 刪除已下載的版本：confirm 列出要刪的版本，仍被設定檔固定使用的一併提醒。 */
function removeVersions(region: Region, targets: InstalledVersion[]): void {
  const entries = targets.map((v) => {
    const label = dalamudVersionLabel(region, v.version, v.dalamudVersion);
    const pinnedBy = config.profiles.filter((p) => {
      const pv = channelVersion(p.channel);
      return channelRegion(p.channel) === region && pv?.kind === "pinned" && pv.tag === v.version;
    });
    const names = pinnedBy.map((p) => `「${p.name}」`).join("、");
    return { label, warning: names ? `${names}設定檔固定使用此版本，刪除後登入時會重新下載。` : "" };
  });
  const [first] = entries;
  const question =
    entries.length === 1
      ? `刪除${regionLabel(region)} ${first.label}？${first.warning ? `\n${first.warning}` : ""}`
      : `刪除以下${regionLabel(region)} Dalamud 版本？\n${entries
          .map((e) => `・${e.label}${e.warning ? `\n　${e.warning}` : ""}`)
          .join("\n")}`;
  if (!confirm(question)) return;
  void run("刪除 Dalamud 版本", async () => {
    for (const v of targets) await api.removeInstalledDalamud(v.source, v.version);
    // 刪掉了下拉選的版本：回到預設選擇（已下載的最新版，沒有就最新版）。
    if (targets.some((v) => v.version === twInstallChoice)) twInstallChoice = null;
    return entries.length === 1
      ? `已刪除${regionLabel(region)} ${first.label}`
      : `已刪除 ${entries.length} 個${regionLabel(region)}版本`;
  }).then(() => document.getElementById("dalamud-version-more")?.focus());
}

/** 「設定檔」區（同「版本」區的版式）：設定檔卡片（編輯中的原位換成表單；新設定檔插在最後）與「新增空白設定檔」。 */
function profileBlock(region: Region, all: DalamudChoice[]): HTMLElement {
  const d = profileDraft?.region === region ? profileDraft : null;
  duplicateProgress = null;
  const cards = all.map((c) => (d && c.profile?.id === d.id ? profileEditor(d, c.profile) : choiceCard(c)));
  if (d?.id === "") cards.push(profileEditor(d, null));
  // 給沒有舊資料的人用，低調放在清單下方。
  const add = h("button", { id: "profiles-add", class: "ghost", type: "button" }, "新增空白設定檔");
  add.hidden = d?.id === "";
  add.addEventListener("click", () => editProfile(region, null));
  const empty =
    detected === null ? `沒有${regionLabel(region)}的 Dalamud 設定檔。` : `沒有偵測到${LAUNCHER[region]}的 Dalamud 資料。`;
  return h(
    "section",
    { class: "dalamud-block", "aria-labelledby": "dalamud-profiles-title" },
    h("h3", { class: "subsection-title", id: "dalamud-profiles-title" }, "設定檔"),
    ...(all.length ? [] : [h("p", { class: "hint" }, empty)]),
    ...cards,
    h("div", { class: "toolbar" }, add),
  );
}

// ---------- 執行狀態：header 狀態列；ACT 子分頁共用同一份結果 ----------

/** 視窗可見時多久查一次；每次會列舉一次系統程序（ACT 判定）。 */
const RUNNING_STATUS_POLL_MS = 5000;
const runningStatusBar = document.getElementById("running-status") as HTMLElement;
/** 最近一次查到的各區服 ACT 狀態；null＝還沒查到。查詢失敗沿用上次的結果。 */
let actStatus: ActStatusByRegion | null = null;
/** 最近一次查到、本程式啟動且仍在執行的遊戲。 */
let runningGames: RunningGame[] = [];
let runningStatusSeq = 0;
/** 輪詢計時器；undefined＝停止（視窗隱藏時）。 */
let runningStatusTimer: number | undefined;
/** header 目前畫出的內容；沒變就不重建，免得 role=status 每次輪詢都重念。 */
let runningStatusShown = "";

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
function paintRunningStatus(): void {
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
function refreshRunningStatus(): void {
  const seq = ++runningStatusSeq;
  void api.getRunningStatus().then(
    (s) => {
      if (seq !== runningStatusSeq) return;
      actStatus = s.act;
      runningGames = s.games;
      paintRunningStatus();
      paintActStatus();
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

// ---------- ACT ----------

/** ACT 設定（國際服、繁中服各一組）；render() 時重讀，null＝讀取失敗（原因已顯示在訊息列）。 */
let actSettings: ActConfig | null = null;
/** ACT 連動狀態（各 ACT 是否已登記外掛、雙開 ACT）；render() 時重讀，null＝讀取失敗。 */
let actLink: ActLinkStatus | null = null;
/** 兩服 ACT 下拉選單的選項；render() 時重讀（建立雙開、儲存設定後立即出現並選中），null＝讀取失敗。 */
let actChoices: ActChoice[] | null = null;
/** 各區服「其他（自行輸入路徑）」尚未儲存的輸入；null＝沒有在輸入，下拉選單顯示已存的選擇。重繪沿用，儲存成功或改選清單項目後清掉。 */
const actPathDraft: Record<Region, string | null> = { global: null, tw: null };
/** ACT 頁的區服子分頁；null＝跟登入面板目前帳號的區服（登入面板換帳號時重設）。與 Dalamud 頁的 tabRegion 互不影響。 */
let actTabRegion: Region | null = null;
/** renderAct() 時重建：子分頁上的 ACT 狀態標記（兩服）與目前面板的狀態文字、「立即開啟 ACT」，狀態回來後就地更新。 */
let actTabMarks: Partial<Record<Region, HTMLElement>> = {};
let actStatusView: { region: Region; text: HTMLElement; start: HTMLButtonElement } | null = null;

const ACT_STATUS_TEXT: Record<ActStatus, string> = {
  notConfigured: "尚未選擇 ACT",
  missing: "找不到設定的執行檔",
  running: "執行中",
  stopped: "未執行",
};

/** 子分頁標記用的短字。 */
const ACT_TAB_TEXT: Record<ActStatus, string> = {
  notConfigured: "未設定",
  missing: "找不到",
  running: "執行中",
  stopped: "未執行",
};

/** 狀態不允許開啟（沒有路徑、執行檔不見）時停用「立即開啟 ACT」；只停用不啟用，啟用交給重繪（run() 期間不解鎖）。 */
function paintActStatus(): void {
  for (const region of REGIONS) {
    const mark = actTabMarks[region];
    if (!mark) continue;
    const status = actStatus ? actStatus[region] : null;
    mark.className = `subtab-status ${status ?? "checking"}`;
    mark.textContent = status ? ACT_TAB_TEXT[status] : "檢查中";
  }
  const v = actStatusView;
  if (!v) return;
  const status = actStatus ? actStatus[v.region] : null;
  v.text.className = `act-state ${status ?? "checking"}`;
  v.text.textContent = status ? ACT_STATUS_TEXT[status] : "檢查中…";
  if (status === "notConfigured" || status === "missing") v.start.disabled = true;
}

/** 各區服 ACT 的 FFXIV 插件版本；null＝還沒查到。 */
let actPlugins: ActPluginVersions | null = null;
/** 查詢 FFXIV 插件版本失敗的原因（不是最新版查不到，那只會讓 latest 為 null）；查到時清掉。 */
let actPluginsError: string | null = null;
let actPluginsLoading = false;
let actPluginsSeq = 0;
/**
 * 上次查詢時兩服的 ACT 路徑；null＝不在 ACT 頁。進 ACT 頁或換了 ACT 才查，重繪（含切區服子分頁、動作後的 render）
 * 不重查；最新版後端另有一小時快取，不加輪詢。
 */
let actPluginsKey: string | null = null;
/** renderAct() 時重建：目前面板的插件版本文字與「檢查插件更新」，查詢回來後就地更新。 */
let actPluginView: { region: Region; text: HTMLElement; recheck: HTMLButtonElement } | null = null;

const ACT_PLUGIN_TEXT: Record<Exclude<ActPluginStatus["state"], "installed">, string> = {
  notConfigured: "FFXIV 插件：尚未選擇 ACT",
  exeMissing: "FFXIV 插件：找不到 ACT 執行檔",
  neverOpened: "FFXIV 插件：這個 ACT 還沒開過，開過一次後才看得到版本",
  unreadable: "FFXIV 插件：讀不懂這個 ACT 的設定檔",
  notInstalled: "FFXIV 插件：這個 ACT 還沒裝 FFXIV 插件",
  versionUnknown: "FFXIV 插件：讀不到版本",
};

/** region 那一服的插件版本一行；有新版時用 warn。 */
function actPluginLine(region: Region): { text: string; cls: string } {
  if (actPluginsLoading) return { text: "FFXIV 插件：檢查中…", cls: "" };
  if (!actPlugins) {
    return { text: actPluginsError ? `無法讀取 FFXIV 插件版本：${actPluginsError}` : "FFXIV 插件：檢查中…", cls: "" };
  }
  const s = actPlugins[region];
  if (s.state !== "installed") return { text: ACT_PLUGIN_TEXT[s.state], cls: "" };
  const latest = actPlugins.latest;
  if (latest === null) return { text: `FFXIV 插件 ${s.version}、無法取得最新版`, cls: "" };
  return s.outdated
    ? { text: `FFXIV 插件 ${s.version}、最新版 ${latest}：開啟 ACT 時會自動更新`, cls: "warn" }
    : { text: `FFXIV 插件 ${s.version}、已是最新版`, cls: "" };
}

function paintActPlugin(): void {
  const v = actPluginView;
  if (!v) return;
  const line = actPluginLine(v.region);
  v.text.className = line.cls;
  v.text.textContent = line.text;
  v.recheck.disabled = actPluginsLoading;
}

/** 查各區服的插件版本；force＝「檢查插件更新」（後端略過最新版的快取）。較晚發出的查詢優先，失敗只顯示在這一行。 */
function loadActPlugins(force: boolean): void {
  const seq = ++actPluginsSeq;
  actPluginsLoading = true;
  paintActPlugin();
  void api.getActPluginVersions(force).then(
    (v) => {
      if (seq !== actPluginsSeq) return;
      actPlugins = v;
      actPluginsError = null;
      actPluginsLoading = false;
      paintActPlugin();
    },
    (e: unknown) => {
      if (seq !== actPluginsSeq) return;
      actPluginsError = errorText(e);
      actPluginsLoading = false;
      paintActPlugin();
    },
  );
}

/** renderAct() 時呼叫：在 ACT 頁、且是剛進來或兩服的 ACT 換了，才查一次；離開 ACT 頁就重設，下次進來再查。 */
function syncActPlugins(s: ActConfig): void {
  if (currentTab !== "extensions" || extTab !== "act") {
    actPluginsKey = null;
    return;
  }
  const key = JSON.stringify(REGIONS.map((r) => s[r].path));
  if (key === actPluginsKey) return;
  actPluginsKey = key;
  loadActPlugins(false);
}

/** ACT 下拉選單「其他（自行輸入路徑）…」的值；清單項目的值是索引，「未設定」是空字串。 */
const ACT_OTHER = "other";

/** 下拉選單項目的白話名稱。 */
function actChoiceLabel(c: ActChoice): string {
  switch (c.kind) {
    case "installed":
      return "原本安裝的 ACT";
    case "dual":
      return `XLM 建立的雙開 ACT（${regionLabel(c.region)}）`;
    case "custom":
      return c.exists ? "自訂路徑" : "自訂路徑（找不到檔案）";
  }
}

/** 儲存 region 那組 ACT 的路徑（null＝未設定），保留「登入後自動開啟」；沒有路徑就開不了 ACT，清空時一併關掉。 */
function saveActPath(region: Region, s: ActConfig[Region], path: string | null, focusId: string): void {
  const label = regionLabel(region);
  const turnsOff = path === null && s.launchWithGame;
  void run(`儲存${label} ACT`, async () => {
    await api.saveActSettings(region, path, path !== null && s.launchWithGame);
    actPathDraft[region] = null;
    return path
      ? `已儲存${label} ACT`
      : turnsOff
        ? `已清除${label} ACT，並關閉登入後自動開啟`
        : `已清除${label} ACT`;
  }).then(() => document.getElementById(focusId)?.focus());
}

/** 一個區服的面板「ACT」：下拉選擇、狀態與「立即開啟 ACT」、FFXIV 插件版本、這個 ACT 的連動狀態、登入後自動開啟。 */
function actRegionPanel(region: Region, s: ActConfig[Region]): HTMLElement[] {
  const label = regionLabel(region);
  const choices = actChoices ?? [];
  const labels = choices.map(actChoiceLabel);
  const current = choices.findIndex((c) => c.selectedBy.includes(region));
  // 讀不到選項清單時，已存的路徑只能在「其他」顯示。
  const value =
    actPathDraft[region] !== null ? ACT_OTHER : current >= 0 ? String(current) : s.path === null ? "" : ACT_OTHER;
  const selectId = `act-choice-${region}`;
  const select = h(
    "select",
    { id: selectId, "aria-label": `${label}要用的 ACT` },
    option("", "未設定", value === ""),
    ...choices.map((c, i) => {
      // 同名項目（例如裝了兩份 ACT）接上路徑區分。
      const text = labels.filter((l) => l === labels[i]).length > 1 ? `${labels[i]}：${c.path}` : labels[i];
      const o = option(String(i), text, value === String(i));
      o.title = c.path;
      return o;
    }),
    option(ACT_OTHER, "其他（自行輸入路徑）…", value === ACT_OTHER),
  );
  const chosen = h("div", { class: "path" }, current >= 0 ? choices[current].path : "");
  chosen.hidden = value === "" || value === ACT_OTHER;

  const input = h("input", {
    id: `act-path-${region}`,
    value: actPathDraft[region] ?? s.path ?? "",
    "aria-label": `${label} ACT 執行檔路徑`,
    placeholder: "例：C:\\Program Files (x86)\\Advanced Combat Tracker\\Advanced Combat Tracker.exe",
    autocomplete: "off",
    spellcheck: "false",
  });
  input.addEventListener("input", () => (actPathDraft[region] = input.value));
  const save = h("button", { class: "primary", type: "button" }, "儲存");
  save.addEventListener("click", () => saveActPath(region, s, input.value.trim() || null, selectId));
  const custom = h("div", { class: "act-path" }, input, save);
  custom.hidden = value !== ACT_OTHER;

  select.addEventListener("change", () => {
    if (select.value === ACT_OTHER) {
      actPathDraft[region] = input.value;
      chosen.hidden = true;
      custom.hidden = false;
      input.focus();
      return;
    }
    actPathDraft[region] = null;
    saveActPath(region, s, select.value === "" ? null : choices[Number(select.value)].path, selectId);
  });

  const autoId = `act-launch-with-game-${region}`;
  const auto = h("input", { id: autoId, type: "checkbox" });
  auto.checked = s.launchWithGame;
  auto.disabled = s.path === null;
  auto.addEventListener("change", () => {
    const on = auto.checked;
    void run(on ? `開啟登入${label}後自動開啟 ACT` : `關閉登入${label}後自動開啟 ACT`, async () => {
      await api.saveActSettings(region, s.path, on);
      return on ? `登入${label}遊戲後會自動開啟這個 ACT` : `登入${label}遊戲後不再自動開啟 ACT`;
    }).then(() => document.getElementById(autoId)?.focus());
  });

  const text = h("span", { class: "act-state", role: "status" });
  const startId = `act-start-${region}`;
  const start = h("button", { id: startId, class: "primary", type: "button" }, "立即開啟 ACT");
  start.disabled = s.path === null;
  start.addEventListener("click", () => {
    void run(`開啟${label} ACT`, async () =>
      (await api.launchAct(region)) === "started" ? `已開啟${label} ACT` : `${label} ACT 已在執行中，未再開啟`,
    ).then(() => document.getElementById(startId)?.focus());
  });
  const recheck = h("button", { id: `act-recheck-${region}`, class: "ghost", type: "button" }, "重新檢查");
  recheck.addEventListener("click", refreshRunningStatus);
  actStatusView = { region, text, start };

  const pluginText = h("span", { role: "status" });
  const pluginRecheck = h(
    "button",
    { id: `act-plugin-recheck-${region}`, class: "ghost", type: "button" },
    "檢查插件更新",
  );
  pluginRecheck.addEventListener("click", () => loadActPlugins(true));
  actPluginView = { region, text: pluginText, recheck: pluginRecheck };

  return [
    h(
      "section",
      { class: "act-block", "aria-labelledby": "act-choice-title" },
      h("h3", { class: "subsection-title", id: "act-choice-title" }, "ACT"),
      h("div", { class: "act-choice" }, select, chosen, custom),
      h("div", { class: "act-status-bar" }, text, start, recheck),
      h("div", { class: "act-status-bar" }, pluginText, pluginRecheck),
      ...actLinkRows(region),
      h("label", { class: "check" }, auto, `登入${label}遊戲後自動開啟 ACT`),
      h(
        "p",
        { class: "field-hint" },
        s.path === null
          ? `先選擇${label}要用的 ACT。`
          : "只對勾了「ACT 跟這個帳號」的帳號有效（設定在登入頁「進階」）。已在執行就不再開；ACT 要求系統管理員時，Windows 會先詢問是否允許。",
      ),
    ),
  ];
}

const ACT_LINK_TEXT: Record<ActLinkState, string> = {
  exeMissing: "找不到執行檔，請在上方重新選擇 ACT",
  neverOpened: "這個 ACT 還沒開過，請先開一次再關閉",
  unreadable: "讀不懂這個 ACT 的設定檔",
  notRegistered: "尚未啟用連動，按下方「啟用 ACT 連動」即可",
  disabled: "連動在 ACT 裡被取消勾選了，按下方「啟用 ACT 連動」可恢復",
  registered: "已啟用連動",
};

/** 這一服選的 ACT 的連動狀態（ACT 卡片裡一行）；兩服選同一個 ACT 時註明單開。 */
function actLinkRows(region: Region): HTMLElement[] {
  const link = actLink;
  if (link === null) return [h("p", { class: "hint" }, "無法讀取連動狀態，原因見下方訊息列。")];
  const a = link.acts.find((x) => x.regions.includes(region));
  if (!a) return [h("p", { class: "hint" }, "選好 ACT 後才能啟用連動。")];
  const rows = [h("p", { class: `act-link-state ${a.state}` }, ACT_LINK_TEXT[a.state])];
  if (a.regions.length > 1) {
    rows.push(h("p", { class: "hint" }, "與另一服共用同一個 ACT（單開：ACT 只會跟最後登入的遊戲）。"));
  }
  return rows;
}

const ACT_CHANGE_TEXT: Record<ActChange["change"], string> = {
  added: "已啟用連動",
  reenabled: "已重新啟用連動",
  unchanged: "原本就已啟用",
};

function actWho(regions: Region[]): string {
  return `${regions.map(regionLabel).join("、")} ACT`;
}

/** 「啟用 ACT 連動」的結果訊息：每個 ACT 一句；有改設定檔時提醒下次開 ACT 才生效。 */
function actChangeText(changes: ActChange[]): string {
  const parts = changes.map((c) => `${actWho(c.regions)}${ACT_CHANGE_TEXT[c.change]}`);
  const changed = changes.some((c) => c.change !== "unchanged");
  return `${parts.join("；")}${changed ? "。下次開啟 ACT 後生效" : ""}`;
}

/**
 * 區服卡片下方的共用設定「ACT 連動與雙開」：一鍵讓各 ACT 自動切到剛開的遊戲，以及一鍵建立另一服用的 ACT。
 * 兩服共用、與區服子分頁同一層：用頁面層級的標題。外掛如何運作收在「運作方式」裡，平常不佔版面；
 * 所有 ACT 都已啟用連動時，「啟用 ACT 連動」退成次要按鈕（仍可按，新加的 ACT 要再登記一次）。
 */
function actSharedBlock(): HTMLElement {
  const link = actLink;
  const instances = link?.instances ?? [];
  const dualText =
    instances.length === 0
      ? "還沒有建立。"
      : instances.map((i) => `已建立${regionLabel(i.region)}用的 ACT${i.inUse ? "" : "（目前沒有選用）"}`).join("；") +
        "。";
  const noAct = link === null || link.acts.length === 0;
  const allLinked = !noAct && link.acts.every((a) => a.state === "registered");

  const enable = h(
    "button",
    { id: "act-link-enable", class: allLinked ? "secondary" : "primary", type: "button" },
    "啟用 ACT 連動",
  );
  enable.disabled = noAct;
  enable.addEventListener("click", () => {
    void run("啟用 ACT 連動", async () => actChangeText(await api.enableActLink())).then(() =>
      document.getElementById("act-link-enable")?.focus(),
    );
  });
  const dual = h("button", { id: "act-link-dual", class: "secondary", type: "button" }, "建立另一服的 ACT（雙開）");
  dual.disabled = noAct;
  dual.addEventListener("click", () => {
    void run("建立另一服的 ACT", async () => {
      const r = await api.createDualAct();
      const other = regionLabel(r.instanceRegion);
      const main = regionLabel(r.mainRegion);
      return r.created
        ? `已建立${other}用的 ACT；原本的 ACT 改給${main}用，兩個都已啟用連動`
        : `${other}用的 ACT 已存在；已把原本的 ACT 設給${main}、這個設給${other}，兩個都已啟用連動`;
    }).then(() => document.getElementById("act-link-dual")?.focus());
  });
  const actOpen = REGIONS.some((r) => actStatus?.[r] === "running");

  return h(
    "section",
    { class: "act-block act-shared", "aria-labelledby": "act-shared-title" },
    h("h3", { class: "section-title", id: "act-shared-title" }, "ACT 連動與雙開"),
    h(
      "p",
      { class: "hint" },
      "啟用連動後，勾了「ACT 跟這個帳號」的帳號登入時，ACT 會自動切到剛開的遊戲；雙開會以原本的 ACT 為底，複製一份給另一服用，建好後自動選上。",
    ),
    h("p", { class: "hint" }, `雙開：${dualText}`),
    h("div", { class: "act-link-actions" }, enable, dual),
    h(
      "p",
      { class: actOpen ? "field-hint act-link-warn" : "field-hint" },
      `${actOpen ? "ACT 正在執行：" : ""}設定前請先關閉所有 ACT（ACT 關閉時會把設定蓋回去）；雙開前請先在原本 ACT 的 FFXIV 插件選好 Region。`,
    ),
    h(
      "details",
      { class: "editor-advanced act-link-how" },
      h("summary", {}, "運作方式"),
      h(
        "div",
        { class: "editor-advanced-body" },
        h(
          "p",
          { class: "hint" },
          "連動靠本程式附的 ACT 外掛 XlmActBridge.cs：登入後它把 FFXIV 插件的「FFXIV Process ID」選到剛開的遊戲，並依這個 ACT 的 Region 只跟國際服或繁中服其中一邊。Region 由各 ACT 自己固定（雙開時會自動設好），外掛不會改 Region。",
        ),
      ),
    ),
  );
}

function renderAct(): void {
  actTabMarks = {};
  actStatusView = null;
  actPluginView = null;
  const s = actSettings;
  if (!s) {
    extPanels.act.replaceChildren(h("p", { class: "hint" }, "無法讀取 ACT 設定，原因見下方訊息列。"));
    return;
  }
  const current = actTabRegion ?? draft?.region ?? "global";
  const tablist = subtabs({
    label: "區服",
    tabs: REGIONS,
    current,
    id: (r) => `act-tab-${r}`,
    controls: () => "act-panel",
    className: (r) => `subtab ${r}`,
    content: (r) => {
      const mark = h("span", { class: "subtab-status" });
      actTabMarks[r] = mark;
      return [regionLabel(r), mark];
    },
    select: (r) => {
      actTabRegion = r;
      renderAct();
      document.getElementById(`act-tab-${r}`)?.focus();
    },
  });
  extPanels.act.replaceChildren(
    h("p", { class: "hint" }, "ACT 用來看傷害統計；國際服、繁中服可以各用一個 ACT，也可以兩服共用一個。"),
    tablist,
    h(
      "div",
      { id: "act-panel", role: "tabpanel", "aria-labelledby": `act-tab-${current}` },
      ...actRegionPanel(current, s[current]),
    ),
    actSharedBlock(),
  );
  paintActStatus();
  paintActPlugin();
  syncActPlugins(s);
}

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
const UPDATE_CHECK_TICK_MS = 5 * 60 * 1000;
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

const versionButton = document.getElementById("app-version") as HTMLButtonElement;
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

function paintAppUpdate(): void {
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
    config = await api.getConfig();
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
function backgroundUpdateCheck(): void {
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

// ---------- 遊戲更新 ----------

/** 各區服的更新中斷標記與待套用更新；render() 時重讀，null＝讀取失敗（原因已顯示在訊息列）。 */
let patchState: PatchState | null = null;
/**
 * 進行中的遊戲更新：進度畫在登入面板的更新區。run() 期間按鈕全鎖，同時至多一筆。
 * 進度狀態放這裡而非 DOM，因為 render() 會重建面板。
 */
let gameUpdating: { region: Region; progress: ProgressState | null } | null = null;
/** renderLogin() 時重建：更新中的進度元素。 */
let gameUpdateProgress: ProgressView | null = null;
/** 國際服更新完成、尚未重新登入成功：之後的登入失敗要註明更新本身已完成。只存記憶體。 */
let reloginPending = false;

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

function startGameUpdate(region: Region): void {
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
function autoCheckTwGameUpdate(): void {
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
function patchStatus(region: Region): HTMLElement[] {
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
function patchActions(region: Region): MenuItem[] {
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
let integrityCheck: { progress: ProgressState | null } | null = null;
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

// ---------- 免責與安全 ----------

/** 一條說明；陣列的第二項是子清單。 */
type SafetyItem = string | [string, string[]];

/**
 * 「設定 › 免責與安全」的三組條列。安全性的每一條都對應程式碼的實際行為（secret：secrets.rs；
 * 連線：global／tw／dalamud／patch 的 URL、act_plugin.rs 與 tauri.conf.json updater；核對：dalamud/install.rs、
 * patch/list.rs、update.rs；提權：patch/helper.rs、patch/access.rs 與 update.rs 的 apply_route；Dalamud 停用：launch.rs 的 Lease 判定；ACT：act_bridge.rs、
 * resources/act/XlmActBridge.cs），改這些行為時同步改這裡。不寫無法由程式碼證實的保證。
 */
const SAFETY_GROUPS: readonly { title: string; items: readonly SafetyItem[] }[] = [
  {
    title: "免責聲明",
    items: [
      "本程式不是 Square Enix 或宇峻奧汀（繁中服營運商）的官方工具，與兩者無關，也未經其認可。",
      "使用非官方啟動器、Dalamud、ACT 等第三方工具，可能違反遊戲的服務條款或使用者合約；因此造成的帳號處分或其他損失，由使用者自行承擔。",
      "本程式依現狀提供，不保證沒有錯誤、適合特定用途或持續可用；遊戲或第三方來源改版時，部分功能可能暫時無法使用。",
    ],
  },
  {
    title: "第三方擴充功能的風險",
    items: [
      "Dalamud 與它的插件是第三方程式，注入後在遊戲程序裡執行，權限與遊戲相同：可以讀寫遊戲記憶體，也能存取你的檔案與網路。",
      "插件的功能、安全性與是否違反條款，由各插件作者負責；本程式不審查插件。只安裝信任來源的插件，自行加入的第三方插件庫風險更高。",
      "國際服的 Dalamud 來自 goatcorp，繁中服的 Dalamud 來自 GitHub 上的 yanmucorp；本程式只負責下載、核對與注入，Dalamud 的內容由各來源負責。",
      "goatcorp 可以遠端暫停國際服 Dalamud（例如遊戲改版後）。暫停中或無法確認狀態時，本程式不注入國際服 Dalamud，登入會停下並說明原因。",
      "ACT 與 FFXIV_ACT_Plugin 也是第三方程式；ACT 通常要求以系統管理員身分執行（開啟時 Windows 會跳 UAC），請只從官方來源取得。",
      "「ACT 連動」外掛（XlmActBridge.cs，原始碼隨本程式附帶）只讀取本程式寫的指令檔，只切換 FFXIV 插件的 Process ID；不讀寫遊戲記憶體、不改其他設定、不寫任何檔案。",
      "「啟用 ACT 連動」會在 ACT 設定檔的外掛清單登記這個外掛；「建立另一服的 ACT」會把 ACT 複製到 %LOCALAPPDATA%\\xiv-launch-manager\\act\\ 並修改副本的設定。第一次修改某個 ACT 設定檔前，會在同資料夾留一份 .xlm-backup 備份。",
      "社群慣例：不要在遊戲內（聊天、招募、對話）提起 DPS、解析數據或插件，也不要拿來評論其他玩家；這類發言容易被檢舉。",
    ],
  },
  {
    title: "本程式的安全性",
    items: [
      "密碼與 OTP 金鑰存在 Windows 認證管理員（Credential Manager），不寫進設定檔或 log，介面也讀不回來。限制：同一個 Windows 使用者下執行的其他程式仍可能讀取，請勿在不信任的電腦上儲存。",
      [
        "本程式只連線到下列服務，不傳送使用資料或統計（沒有遙測）：",
        [
          "Square Enix：國際服登入、版本檢查、遊戲更新、伺服器狀態、官方公告（frontier.ffxiv.com，只讀標題與日期；點開才連到 Lodestone）",
          "繁中服官方伺服器：登入、版本檢查、遊戲更新；官網（www.ffxiv.com.tw）：伺服器狀態、最新消息；登入時的 reCAPTCHA 由 Google 提供",
          "goatcorp：國際服 Dalamud 與其 .NET runtime、assets、遠端暫停狀態、遊戲檔完整性對照",
          "GitHub 上的 yanmucorp：繁中服 Dalamud 與 assets；NuGet：繁中服 Dalamud 需要的 .NET runtime",
          "GitHub：本程式的更新",
          "GitHub 上的 ravahn/FFXIV_ACT_Plugin：查詢 FFXIV 插件的最新版本號（只查版本、不下載；本程式開著時，距上次查詢未滿一小時就沿用上次結果，按「檢查插件更新」才重查）",
          "以及上述來源清單裡列出的下載位置（例如 Dalamud assets 的個別檔案）",
        ],
      ],
      [
        "下載的檔案會先核對再使用，不符就不使用：",
        [
          "Dalamud 本體、assets 與國際服的 .NET runtime：依來源公布的 hash 清單逐檔核對（繁中服的 .NET runtime 來源沒有提供 hash，只靠 HTTPS 傳輸）",
          "遊戲更新檔：依官方更新清單的分段 SHA1 核對，下載後與套用前各一次。國際服的清單經 HTTPS 取得。繁中服的版本檢查（含清單）只能走 HTTP，更新檔則一律經 HTTPS 從官方下載主機 patch-dl.ffxiv.com.tw 取得，不跟隨轉到其他主機或降級成 HTTP 的轉址；清單在傳輸途中被竄改時，可能誤判是否需要更新、讓更新失敗，或套用順序、版本不對的官方更新檔（之後需用官方啟動器修復），但不會下載官方主機以外的檔案",
          "本程式的更新：以內建公鑰驗證 minisign 簽章，驗證不過不安裝；免安裝版在執行檔所在的資料夾暫存新版，驗證通過才替換執行檔",
          "核對只能確認檔案與來源公布的一致、下載途中沒有損壞或被竄改，不能證明來源本身安全",
        ],
      ],
      "本程式以一般使用者權限執行，不要求系統管理員。國際服遊戲更新只在安裝資料夾受保護（一般權限不能修改，例如裝在 Program Files）時才跳 UAC：同意後執行的是本程式自己的更新 helper，只負責核對與套用更新檔、寫入版本檔，不開視窗，完成即結束；helper 發現安裝資料夾一般權限就能修改時不寫入。國際服安裝資料夾本程式就能修改時，以及繁中服，更新都在本程式內套用，不跳 UAC。",
      "國際服的「檢查遊戲檔完整性」只讀取比對，不修改遊戲檔。",
      "請只從官方 GitHub Releases（github.com/Yshawn6720/xiv-launch-manager/releases）下載本程式；其他來源的檔案可能被改過。",
    ],
  },
];

function safetyItem(item: SafetyItem): HTMLElement {
  if (typeof item === "string") return h("li", {}, item);
  const [text, sub] = item;
  return h("li", {}, text, h("ul", {}, ...sub.map((s) => h("li", {}, s))));
}

/** 「授權與第三方聲明」裡的文件；全文由後端內嵌提供（commands.rs `get_license_text`）。 */
const LICENSE_DOCS: readonly { doc: LicenseDoc; title: string }[] = [
  { doc: "license", title: "授權條款（LICENSE，Apache License 2.0）" },
  { doc: "notice", title: "版權與免責聲明（NOTICE）" },
  { doc: "thirdPartyNotices", title: "第三方元件授權（THIRD-PARTY-NOTICES.txt）" },
];

/** 一份授權文件：第一次展開才向後端取全文（第三方授權約 570 KB，不在啟動時傳）；取不到就顯示原因，下次展開再取。 */
function licenseDocView(doc: LicenseDoc, title: string): HTMLElement {
  // tabindex=0：長文在框內捲動，鍵盤也要能捲。
  const body = h("pre", { class: "license-text", tabindex: "0" });
  const view = h("details", { class: "license-doc" }, h("summary", {}, title), body);
  let requested = false;
  view.addEventListener("toggle", () => {
    if (!view.open || requested) return;
    requested = true;
    body.textContent = "載入中…";
    api.getLicenseText(doc).then(
      (text) => {
        body.textContent = text;
      },
      (e: unknown) => {
        requested = false;
        body.textContent = `讀取失敗：${errorText(e)}`;
      },
    );
  });
  return view;
}

/**
 * 「設定 › 免責與安全」：內容不隨狀態變，只建一次；renderSettings() 每次搬回分頁裡，展開狀態跟著保留。
 * 每組用 details 收起，避免一進設定分頁就是一整片文字。
 */
const safetyBlock = h(
  "div",
  { id: "settings-safety", class: "settings-block safety-block" },
  // tabindex=-1：從「擴充功能」的提醒連過來時把焦點移到這裡（螢幕閱讀器會念出標題）。
  h("h3", { class: "section-title", id: "settings-safety-title", tabindex: "-1" }, "免責與安全"),
  ...SAFETY_GROUPS.map((g) =>
    h(
      "details",
      { class: "safety-group" },
      h("summary", {}, g.title),
      h("ul", { class: "safety-list" }, ...g.items.map(safetyItem)),
    ),
  ),
  h(
    "details",
    { class: "safety-group" },
    h("summary", {}, "授權與第三方聲明"),
    h(
      "p",
      { class: "hint license-intro" },
      "本程式以 Apache License 2.0 授權；隨程式發佈的第三方元件與它們的授權，列在「第三方元件授權」。全文內嵌在程式裡，展開即可閱讀。",
    ),
    ...LICENSE_DOCS.map((d) => licenseDocView(d.doc, d.title)),
  ),
);

/** 切到「設定」分頁並捲到「免責與安全」，焦點移到其標題。 */
function showSafety(): void {
  void showTab("settings").then(() => {
    safetyBlock.scrollIntoView({ block: "start" });
    document.getElementById("settings-safety-title")?.focus({ preventScroll: true });
  });
}

// ---------- 設定 ----------

const THEME_TEXT: Record<Theme, string> = { system: "跟隨系統", dark: "深色", light: "淺色" };
const AFTER_LAUNCH_TEXT: Record<AfterLaunch, string> = { keep: "不動作", minimize: "最小化", tray: "縮到系統匣" };

/** 「介面」的寫入依序送出：方向鍵連續切換時，後送的不會被先送的蓋掉。 */
let uiSaving: Promise<void> = Promise.resolve();

/**
 * 儲存「介面」的一項。不走 run()：不鎖按鈕、不重繪，焦點留在 radio 上（方向鍵可連續切換）；失敗時顯示原因並
 * 重繪，選項與主題都退回已存的值。
 */
function saveUi(done: string, save: () => Promise<void>, saved: () => void): void {
  uiSaving = uiSaving.then(save).then(
    () => {
      saved();
      say(done, "ok");
    },
    (e: unknown) => {
      say(errorText(e), "error");
      void render();
    },
  );
}

/** 「介面」的一列：標籤＋segmented（原生 radio，方向鍵切換），選了就呼叫 choose；extra 接在 segmented 下方。 */
function choiceRow<T extends string>(
  id: string,
  label: string,
  texts: Record<T, string>,
  current: T,
  choose: (value: T) => void,
  ...extra: HTMLElement[]
): HTMLElement {
  const labelId = `${id}-label`;
  return h(
    "div",
    { class: "setting-row choice-setting" },
    h("span", { class: "choice-label", id: labelId }, label),
    h(
      "div",
      { class: "segmented", role: "radiogroup", "aria-labelledby": labelId },
      ...(Object.keys(texts) as T[]).map((value) => {
        const radio = h("input", { type: "radio", name: id, value });
        radio.checked = value === current;
        radio.addEventListener("change", () => choose(value));
        return h("label", {}, radio, texts[value]);
      }),
    ),
    ...extra,
  );
}

/** 「介面」：主題（立即套用）與遊戲啟動後的視窗行為（下次成功啟動遊戲時生效）。 */
function uiBlock(): HTMLElement {
  return h(
    "section",
    { class: "settings-block" },
    h("h3", { class: "section-title" }, "介面"),
    choiceRow("settings-theme", "主題", THEME_TEXT, config.ui.theme, (theme) => {
      applyTheme(theme);
      saveUi(`主題改為「${THEME_TEXT[theme]}」`, () => api.setTheme(theme), () => (config.ui.theme = theme));
    }),
    choiceRow(
      "settings-after-launch",
      "遊戲啟動後",
      AFTER_LAUNCH_TEXT,
      config.ui.afterLaunch,
      (afterLaunch) =>
        saveUi(
          `遊戲啟動後改為「${AFTER_LAUNCH_TEXT[afterLaunch]}」`,
          () => api.setAfterLaunch(afterLaunch),
          () => (config.ui.afterLaunch = afterLaunch),
        ),
      h(
        "p",
        { class: "field-hint" },
        "只在本程式成功開出遊戲後動作；本程式開的遊戲全部關閉後，視窗自動還原。縮到系統匣時，左鍵點圖示叫回視窗，右鍵選單可結束程式。",
      ),
    ),
  );
}

function renderSettings(): void {
  const root = section("settings");
  const rows = REGIONS.map((region) => {
    const saved = config.games[region] ?? "";
    const input = h("input", { value: saved, "aria-label": `${regionLabel(region)}遊戲路徑` });
    // 「儲存」只在路徑和已存的不同時可按：兩服各一顆常亮的主要按鈕會搶走注意力。
    const save = h("button", { class: "primary" }, "儲存");
    const syncSave = () => {
      save.disabled = input.value.trim() === saved;
    };
    syncSave();
    input.addEventListener("input", syncSave);
    const detect = h("button", { class: "secondary" }, "自動偵測");
    detect.addEventListener("click", async () => {
      const found = await api.detectGamePath(region);
      if (found) {
        input.value = found;
        syncSave();
      } else say(`找不到${regionLabel(region)}安裝位置，請手動輸入`, "error");
    });
    save.addEventListener("click", () =>
      run(`儲存${regionLabel(region)}路徑`, () => api.setGamePath(region, input.value.trim() || null)),
    );
    // 固定欄寬的 grid：兩區服的輸入框左右緣對齊，不受 badge 文字寬度影響。
    return h(
      "div",
      { class: "setting-row path-setting" },
      h("span", { class: `badge ${region}` }, regionLabel(region)),
      input,
      detect,
      save,
    );
  });
  const openData = h("button", { class: "secondary" }, "開啟資料夾");
  openData.addEventListener("click", () => openFolder({ kind: "dataRoot" }));
  // 每一組是一張區塊卡片：標題（與說明）在上，設定列在下。程式更新的入口只在標題列（版本號＋更新按鈕），這裡不重複。
  root.replaceChildren(
    h(
      "section",
      { class: "settings-block" },
      h("h3", { class: "section-title" }, "遊戲路徑"),
      h("p", { class: "hint" }, "填遊戲根目錄（含 boot 與 game 資料夾的那一層）。"),
      ...rows,
    ),
    h(
      "section",
      { class: "settings-block" },
      h("h3", { class: "section-title" }, "資料位置"),
      h(
        "div",
        { class: "setting-row" },
        h("div", { class: "card-main" }, h("div", { class: "path" }, config.dataDir)),
        h("div", { class: "card-actions" }, openData),
      ),
    ),
    uiBlock(),
    safetyBlock,
  );
}

// ---------- 主流程 ----------

let currentTab = "login";

async function render(): Promise<void> {
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
  try {
    config = await api.getConfig();
  } catch (e) {
    say(errorText(e), "error");
    return;
  }
  applyTheme(config.ui.theme);
  detected = await detecting;
  patchState = await patching;
  actSettings = await acting;
  actLink = await linking;
  actChoices = await choosing;
  renderLogin();
  renderExtensions();
  renderSettings();
  paintAppUpdate();
  if (currentTab === "login") refreshServerStatus();
  // 帳號名稱可能改了，先用上次的結果重畫；每個動作（含登入啟動、開啟 ACT）結束後 run() 都會重繪，這裡順便重查。
  paintRunningStatus();
  refreshRunningStatus();
}

/** 切換分頁並重繪。 */
async function showTab(tab: string): Promise<void> {
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

// ---------- 進度 ----------

const MB = 1024 * 1024;
const SPEED_WINDOW_MS = 2000;
/** 同一 stage 內最近約 2 秒的 (時間, 已下載) 取樣，用來算平滑速度。 */
let speedStage = "";
let speedSamples: { t: number; done: number }[] = [];

function resetSpeed(): void {
  speedStage = "";
  speedSamples = [];
}

/** 回傳 bytes/s；取樣不足或時間跨度太短時回傳 null。 */
function sampleSpeed(stage: string, done: number): number | null {
  const last = speedSamples[speedSamples.length - 1];
  if (stage !== speedStage || (last && done < last.done)) {
    speedStage = stage;
    speedSamples = [];
  }
  const now = performance.now();
  speedSamples.push({ t: now, done });
  // 保留一個落在視窗起點之前的取樣，讓跨度接近 2 秒。
  while (speedSamples.length > 2 && now - speedSamples[1].t >= SPEED_WINDOW_MS) {
    speedSamples.shift();
  }
  const first = speedSamples[0];
  const span = now - first.t;
  if (span < 300) return null;
  return ((done - first.done) * 1000) / span;
}

interface ProgressView {
  box: HTMLElement;
  text: HTMLElement;
  bar: HTMLProgressElement;
  cancel: HTMLButtonElement;
  /** true＝只在目前階段可取消時顯示「取消」（登入流程只有下載可取消；遊戲更新只有下載與核對）。 */
  cancelByStage: boolean;
}

interface ProgressState {
  text: string;
  done: number;
  total: number | null;
  /** 這個階段可取消：有下載位元組進度（不是純階段事件），或遊戲更新的核對。 */
  cancellable: boolean;
}

/**
 * 進行中的登入：登入面板承接 accountId 為帳號 id 或該區服 install:<region> 的進度事件。
 * run() 期間按鈕全鎖，同時至多一筆。進度狀態放這裡而非 DOM，因為 render() 會重建面板。
 */
let launching: { accountId: string; region: Region } | null = null;
let launchProgress: ProgressState | null = null;
/** renderLogin() 時重建：登入面板內的進度元素。 */
let loginProgress: ProgressView | null = null;

function progressView(label: string, cancelByStage: boolean): ProgressView {
  const text = h("span");
  const bar = h("progress", { "aria-label": label });
  // progress-cancel 不受 run() 鎖定：下載期間要按得到。
  const cancel = h("button", { class: "secondary progress-cancel", type: "button" }, "取消");
  cancel.addEventListener("click", () => {
    cancel.disabled = true;
    cancel.textContent = "取消中…";
    // 被取消的下載以 CANCELLED_MESSAGE 失敗，由發起下載的 run() 顯示。
    api.cancelDalamudDownload().catch((e: unknown) => {
      cancel.disabled = false;
      cancel.textContent = "取消";
      say(errorText(e), "error");
    });
  });
  const box = h("div", { class: "progress-view" }, text, h("div", { class: "progress-row" }, bar, cancel));
  return { box, text, bar, cancel, cancelByStage };
}

function paintProgress(view: ProgressView, state: ProgressState | null): void {
  view.box.hidden = state === null;
  if (state === null) return;
  view.text.textContent = state.text;
  view.cancel.hidden = view.cancelByStage && !state.cancellable;
  if (state.total) {
    view.bar.max = state.total;
    view.bar.value = state.done;
  } else {
    view.bar.removeAttribute("value");
  }
}

/** 等手動 OTP 時收起面板進度，避免殘留的下載進度看起來像還在跑。 */
function clearLaunchProgress(): void {
  launchProgress = null;
  if (loginProgress) paintProgress(loginProgress, null);
}

function progressState(p: Progress): ProgressState {
  if (p.done === 0 && p.total === null) {
    // 純階段事件（例如「登入台服」）。
    resetSpeed();
    return { text: p.stage, done: 0, total: null, cancellable: false };
  }
  const speed = sampleSpeed(p.stage, p.done);
  const size = p.total !== null ? `${(p.done / MB).toFixed(1)} / ${(p.total / MB).toFixed(1)} MB` : `${(p.done / MB).toFixed(1)} MB`;
  const rate =
    speed === null ? "" : speed >= MB ? ` · ${(speed / MB).toFixed(1)} MB/s` : ` · ${Math.round(speed / 1024)} KB/s`;
  return { text: `${p.stage}　${size}${rate}`, done: p.done, total: p.total, cancellable: true };
}

void listen<ProgressPayload>(PROGRESS_EVENT, (event) => {
  const p = event.payload;
  if (launching && (p.accountId === launching.accountId || p.accountId === installProgressId(launching.region))) {
    launchProgress = progressState(p);
    if (loginProgress) paintProgress(loginProgress, launchProgress);
  } else if (installing && p.accountId === installProgressId(installing.region)) {
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
  if (selectedAccount()?.id === accountId) loginError = { field: null, text };
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
