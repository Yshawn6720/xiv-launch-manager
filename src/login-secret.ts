import { api } from "./api";
import { errorText, fieldError, h, say } from "./app";
import { lockIcon } from "./widgets";

// ---------- 登入表單：密碼／OTP 金鑰欄與 OTP 預覽 ----------

/** stored＝已儲存（唯讀狀態列）、replacing＝按了「更換」、pending＝按了「清除」待刪除、unsaved＝尚未儲存。 */
export type SecretState = "stored" | "replacing" | "pending" | "unsaved";

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
export function secretView(
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
export function otpPreview(source: () => { accountId: string | null; secret: string | null } | null): OtpPreviewView {
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

