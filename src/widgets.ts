import { type Child, h } from "./app";

const SVG_NS = "http://www.w3.org/2000/svg";

/** 鎖頭圖示（currentColor）：closed＝已儲存，open＝未儲存／將刪除。純裝飾，旁邊一定有文字。 */
export function lockIcon(closed: boolean): SVGSVGElement {
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

/**
 * 行內重新命名：標題暫時藏起，原位換成輸入框；Enter／失焦存、Esc 取消（輸入法選字中的交給輸入法）。
 * trigger 是開始改名的控制項（「重新命名」按鈕或放著它的「⋯」選單），編輯期間藏起。
 * 名稱有變才呼叫 save（留空採 fallback）；沒變或取消時焦點回到 trigger。
 */
export function renameInPlace(
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

export interface MenuItem {
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
export function moreMenu(id: string, groups: MenuItem[][], icon = false): HTMLDetailsElement {
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

// ---------- 子分頁 ----------

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
export function subtabs<T extends string>(s: SubtabSpec<T>): HTMLElement {
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
