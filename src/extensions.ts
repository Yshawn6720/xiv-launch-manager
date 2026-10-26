import { renderAct } from "./act";
import { h, section } from "./app";
import { renderDalamud } from "./dalamud";
import { showSafety } from "./settings";
import { subtabs } from "./widgets";

// ---------- 擴充功能分頁 ----------

type ExtTab = "dalamud" | "act";
const EXT_TABS: readonly ExtTab[] = ["dalamud", "act"];
const EXT_TAB_TEXT: Record<ExtTab, string> = { dalamud: "Dalamud", act: "ACT" };
/** 「擴充功能」目前的子分頁；只存記憶體。 */
export let extTab: ExtTab = "dalamud";

/**
 * 子分頁的面板：元素只建一次，renderDalamud()／renderAct() 各自重建內容。
 * id 用 ext- 前綴，不與 Dalamud 頁內的區服子分頁（dalamud-tab-*、dalamud-panel）重複。
 */
export const extPanels: Record<ExtTab, HTMLElement> = {
  dalamud: h("div", { id: "ext-panel-dalamud", role: "tabpanel", "aria-labelledby": "ext-tab-dalamud" }),
  act: h("div", { id: "ext-panel-act", role: "tabpanel", "aria-labelledby": "ext-tab-act" }),
};

export function renderExtensions(): void {
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
