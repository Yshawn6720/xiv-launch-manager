import {
  api,
  regionLabel,
  type ActConfig,
  type ActChange,
  type ActChoice,
  type ActLinkState,
  type ActLinkStatus,
  type ActPluginStatus,
  type ActPluginVersions,
  type ActStatus,
  type Region,
} from "./api";
import { errorText, h, option, REGIONS, run } from "./app";
import { extPanels, extTab } from "./extensions";
import { draft } from "./login";
import { currentTab } from "./main";
import { actStatus, refreshRunningStatus } from "./running-status";
import { subtabs } from "./widgets";

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

/** render() 時換上重讀的 ACT 設定、連動狀態與下拉選項。 */
export function setActData(settings: ActConfig | null, link: ActLinkStatus | null, choices: ActChoice[] | null): void {
  actSettings = settings;
  actLink = link;
  actChoices = choices;
}

/** 登入面板換帳號時，ACT 頁的區服子分頁回到跟著帳號的區服。 */
export function resetActTabRegion(): void {
  actTabRegion = null;
}

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
export function paintActStatus(): void {
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

export function renderAct(): void {
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
