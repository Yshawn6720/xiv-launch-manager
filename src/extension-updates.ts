import { api, regionLabel, type ActConfig, type ExtensionUpdate } from "./api";
import { config, errorText, h } from "./app";
import { showExtension } from "./extensions";
import { currentTab } from "./main";

const CHECK_INTERVAL_MS = 60 * 60 * 1000;
let actSettings: ActConfig | null = null;
let request: { key: string; at: number } | null = null;

const list = h("ul", { "aria-live": "polite" });
export const extensionUpdateGroup = h(
  "section",
  { class: "extension-update-group", "aria-labelledby": "extension-update-title", hidden: "" },
  h("h3", { class: "subsection-title", id: "extension-update-title" }, "擴充功能有更新"),
  list,
);

function paintUpdates(updates: ExtensionUpdate[]): void {
  extensionUpdateGroup.hidden = updates.length === 0;
  list.replaceChildren(...updates.map((update) => {
    const name = update.kind === "act"
      ? `${update.shared ? "兩服共用" : regionLabel(update.region)} ACT FFXIV 插件`
      : `${regionLabel(update.region)} Dalamud`;
    const link = h("button", { class: "link", type: "button" }, `${name} 有新版 ${update.latest}`);
    link.addEventListener("click", () => void showExtension(update.kind, update.region));
    return h(
      "li",
      {},
      link,
      ...(update.kind === "act" ? [h("span", { class: "field-hint" }, "開啟 ACT 時會自動更新")] : []),
    );
  }));
}

export function setExtensionUpdateActSettings(settings: ActConfig | null): void {
  actSettings = settings;
}

/** 設定或已下載版本變動時不再顯示上一份結果；過期同設定可保留到背景查完。 */
export function refreshExtensionUpdates(): void {
  if (!config?.ui.extensionUpdateReminders) {
    request = null;
    paintUpdates([]);
    return;
  }
  const key = JSON.stringify([
    actSettings?.global.path,
    actSettings?.tw.path,
    config.profiles.some((p) => p.channel.source === "yanmucorp" && p.channel.version.kind === "latest"),
    config.installed.map(({ source, version }) => [source, version]),
  ]);
  if (request && request.key !== key) {
    request = null;
    paintUpdates([]);
  }
  if (currentTab !== "login") return;
  const at = Date.now();
  if (request && at - request.at < CHECK_INTERVAL_MS) return;
  const pending = { key, at };
  request = pending;
  void api.getExtensionUpdates().then(
    (updates) => {
      if (request === pending && config.ui.extensionUpdateReminders) paintUpdates(updates);
    },
    (e: unknown) => {
      if (request !== pending) return;
      paintUpdates([]);
      console.warn("背景檢查擴充功能更新失敗", errorText(e));
    },
  );
}
