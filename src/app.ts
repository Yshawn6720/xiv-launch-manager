import {
  api,
  CANCELLED_MESSAGE,
  type AccountView,
  type ConfigView,
  type DetectedDalamudData,
  type FolderTarget,
  type Region,
  type Theme,
} from "./api";
import { render } from "./main";
import { resetSpeed } from "./progress";

// ---------- 主題（設定 › 介面） ----------

/**
 * 主題的鏡像（localStorage）：程式一開就套用、不等讀設定（第一次 render 之前），深淺不會閃一下；render() 讀到
 * 設定後以設定為準。還沒有鏡像（新安裝）時先用預設的深色，同 Rust `Theme` 的預設。只有強制深色／淺色時設
 * data-theme；跟隨系統時拿掉，styles.css 的 color-scheme 跟系統。
 */
const THEME_KEY = "xlm.theme";
const THEMES: readonly Theme[] = ["system", "dark", "light"];

export function applyTheme(theme: Theme): void {
  if (theme === "system") delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = theme;
  localStorage.setItem(THEME_KEY, theme);
}

const mirroredTheme = localStorage.getItem(THEME_KEY);
applyTheme(THEMES.find((t) => t === mirroredTheme) ?? "dark");

// ---------- DOM、訊息列、動作與共用設定 ----------

export type Child = Node | string;

export function h<K extends keyof HTMLElementTagNameMap>(
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

export function option(value: string, label: string, selected: boolean): HTMLOptionElement {
  const o = h("option", { value }, label);
  o.selected = selected;
  return o;
}

let fieldSeq = 0;

/** 表單的一列：label 以 for 關聯 target（預設就是 control 本身），點標籤即聚焦欄位。 */
export function field(text: string, control: HTMLElement, target: HTMLElement = control): HTMLElement[] {
  if (!target.id) target.id = `field-${++fieldSeq}`;
  return [h("label", { for: target.id }, text), control];
}

/** 欄位下方的錯誤：標記 target 為 aria-invalid 並以 aria-describedby 關聯；沒有錯誤時為空。 */
export function fieldError(target: HTMLElement, error: string | null): HTMLElement[] {
  if (!error) return [];
  const id = `${target.id}-error`;
  target.setAttribute("aria-invalid", "true");
  target.setAttribute("aria-describedby", id);
  return [h("p", { class: "field-error", id }, error)];
}

export function section(id: string): HTMLElement {
  const el = document.getElementById(`tab-${id}`);
  if (!el) throw new Error(`缺少 #tab-${id}`);
  return el;
}

const message = document.getElementById("message") as HTMLDivElement;
export const otpDialog = document.getElementById("otp-dialog") as HTMLDialogElement;
export const otpDialogTitle = document.getElementById("otp-dialog-title") as HTMLHeadingElement;
export const otpInput = document.getElementById("otp-input") as HTMLInputElement;

export function say(text: string, kind: "ok" | "error" | "" = ""): void {
  message.textContent = text;
  message.className = kind;
}

export function errorText(e: unknown): string {
  return typeof e === "string" ? e : e instanceof Error ? e.message : String(e);
}

/** 動作完成時的訊息：字串為成功樣式；完成但結果不是成功（例如檢查發現問題）時指定樣式。 */
type RunResult = string | { text: string; kind: "ok" | "error" | "" };

/** 進行中的 run() 數；背景動作（例如台服遊戲更新自動檢查）據此避開正在鎖住面板的動作。 */
export let runsActive = 0;

/** 執行一個會改狀態的動作：期間鎖住按鈕與下拉選單（下載進度的「取消」除外），結束後重繪。 */
export async function run(label: string, action: () => Promise<RunResult | void>): Promise<void> {
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

export let config: ConfigView;
/** 偵測到的舊啟動器 Dalamud 資料；null＝偵測失敗（錯誤已顯示在訊息列）。 */
export let detected: DetectedDalamudData[] | null = null;

/** render() 讀到設定後換上；更新程式前也會重讀一次。 */
export function setConfig(next: ConfigView): void {
  config = next;
}

export function setDetected(next: DetectedDalamudData[] | null): void {
  detected = next;
}

export const REGIONS: Region[] = ["global", "tw"];

/** 開資料夾不走 run()：不鎖按鈕、不重繪，失敗只顯示訊息。 */
export function openFolder(target: FolderTarget): void {
  api.openFolder(target).catch((e: unknown) => say(errorText(e), "error"));
}

export function hhmm(d: Date): string {
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

// 舊資料可能存了空名稱，退回帳號。
export const accountLabel = (a: AccountView): string => a.name || a.username;
