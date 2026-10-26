import { api, type Progress } from "./api";
import { errorText, h, say } from "./app";

// ---------- 進度 ----------

export const MB = 1024 * 1024;
const SPEED_WINDOW_MS = 2000;
/** 同一 stage 內最近約 2 秒的 (時間, 已下載) 取樣，用來算平滑速度。 */
let speedStage = "";
let speedSamples: { t: number; done: number }[] = [];

export function resetSpeed(): void {
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

export interface ProgressView {
  box: HTMLElement;
  text: HTMLElement;
  bar: HTMLProgressElement;
  cancel: HTMLButtonElement;
  /** true＝只在目前階段可取消時顯示「取消」（登入流程只有下載可取消；遊戲更新只有下載與核對）。 */
  cancelByStage: boolean;
}

export interface ProgressState {
  text: string;
  done: number;
  total: number | null;
  /** 這個階段可取消：有下載位元組進度（不是純階段事件），或遊戲更新的核對。 */
  cancellable: boolean;
}

export function progressView(label: string, cancelByStage: boolean): ProgressView {
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

export function paintProgress(view: ProgressView, state: ProgressState | null): void {
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

export function progressState(p: Progress): ProgressState {
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
