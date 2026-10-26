import {
  api,
  channelRegion,
  channelVersion,
  isRegionInstall,
  regionChannel,
  regionLabel,
  type BindTarget,
  type DetectedDalamudData,
  type InstalledVersion,
  type Profile,
  type Region,
  type YanmucorpVersion,
} from "./api";
import { accountLabel, config, detected, errorText, h, openFolder, option, REGIONS, run, say } from "./app";
import { extPanels } from "./extensions";
import { draft, loginField } from "./login";
import { render } from "./main";
import { paintProgress, progressState, type ProgressState, progressView, type ProgressView } from "./progress";
import { type MenuItem, moreMenu, renameInPlace, subtabs } from "./widgets";

/** 台服可下載的版本（新到舊）；同一次執行成功抓到一次就不再抓。 */
let twVersions: string[] | null = null;
let twVersionsLoad: Promise<string[]> | null = null;
/** 最近一次讀取台服版本清單失敗的原因；不自動重試，版本下拉旁的「重試」可手動重試。 */
let twVersionsError: string | null = null;

// ---------- Dalamud 設定檔 ----------

/**
 * 一個可套用的 Dalamud 設定：舊啟動器資料（可能已有沿用它的設定檔）或本程式的設定檔。
 * 登入面板的「Dalamud 設定檔」下拉與「擴充功能 › Dalamud」的設定檔卡片共用同一份清單。
 */
export interface DalamudChoice {
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
export function fillDalamudSelect(select: HTMLSelectElement, region: Region, current: string | null): Map<string, DalamudChoice> {
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

export function bindTarget(choices: Map<string, DalamudChoice>, value: string): BindTarget {
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
export function dalamudVersionLabel(region: Region, dirVersion: string, dalamudVersion: string | null): string {
  // 國際服的目錄名就是 AssemblyVersion，讀不到檔案版本時拿來頂替。
  if (region === "global") return `Dalamud ${dalamudVersion ?? dirVersion}`;
  const date = twReleaseLabel(dirVersion);
  return dalamudVersion ? `Dalamud ${dalamudVersion}（${date}）` : date;
}

function findInstalled(region: Region, dirVersion: string): InstalledVersion | undefined {
  return config.installed.find((v) => isRegionInstall(v, region) && v.version === dirVersion);
}

/** 台服 tag 的顯示：已下載的附上 Dalamud 版本，否則只有日期標籤。 */
export function twTagLabel(tag: string): string {
  const v = findInstalled("tw", tag);
  return v ? dalamudVersionLabel("tw", tag, v.dalamudVersion) : twReleaseLabel(tag);
}

/**
 * 安裝目錄 dirVersion 的 `Dalamud.dll` 檔案版本，給下載／啟動完成的訊息用。
 * 清單裡還沒有（例如剛下載好）就重讀一次；讀不到為 null，訊息退回只顯示目錄版本。
 */
export async function installedDalamudVersion(region: Region, dirVersion: string): Promise<string | null> {
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
export function twVersionSelect(id: string, current: string): { select: HTMLSelectElement; error: HTMLElement } {
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

// ---------- Dalamud 分頁 ----------

/** 子分頁的區服；null＝跟登入面板目前帳號的區服（登入面板換帳號時重設）。 */
let tabRegion: Region | null = null;

/** 登入面板換帳號時，Dalamud 頁的區服子分頁回到跟著帳號的區服。 */
export function resetDalamudTabRegion(): void {
  tabRegion = null;
}

function currentTabRegion(): Region {
  return tabRegion ?? draft?.region ?? "global";
}

/**
 * 進行中的「下載／更新」：進度依 install:<region> 事件畫在該區服的子分頁。
 * run() 期間按鈕全鎖，同時至多一筆。進度狀態放這裡而非 DOM，因為 render() 會重建面板。
 */
export let installing: { region: Region; progress: ProgressState | null } | null = null;
/** renderDalamud() 時重建：下載中的進度元素。 */
export let installProgress: ProgressView | null = null;
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

export function renderDalamud(): void {
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
