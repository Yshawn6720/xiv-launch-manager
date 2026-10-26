import {
  api,
  regionLabel,
  type CategoryPlan,
  type CharacterKey,
  type CharacterList,
  type CharacterView,
  type Region,
  type RegionCharacters,
  type TransferCategory,
} from "./api";
import {
  accountLabel,
  config,
  errorText,
  field,
  h,
  hhmm,
  openFolder,
  option,
  REGIONS,
  run,
  runsActive,
  section,
} from "./app";
import { draft } from "./login";
import { subtabs } from "./widgets";

// ---------- 角色分頁 ----------

/** 兩服角色；null＝讀取失敗（錯誤已顯示在訊息列）。只在「角色」分頁讀取。 */
let characters: CharacterList | null = null;

export function setCharacters(next: CharacterList | null): void {
  characters = next;
}

/** 角色分頁的區服子分頁；null＝跟登入面板目前帳號的區服。 */
let charRegion: Region | null = null;
/** 展開中的角色列（charKey）；重繪沿用。 */
const charOpen = new Set<string>();
/** 名稱欄未儲存的輸入（charKey → 文字）；其他動作重繪時保留。 */
const charNameDrafts = new Map<string, string>();

const TRANSFER_CATEGORY_TEXT: Record<TransferCategory, string> = {
  hotbar: "熱鍵欄",
  macro: "角色巨集",
  keybind: "按鍵綁定",
  chatFilter: "聊天過濾",
  hud: "HUD 與視窗配置",
  characterSettings: "角色設定",
  controls: "操作設定",
};

/**
 * 轉移卡片的選擇：from／to 為 charKey（""＝未選），picked 為勾選的分類（預設都不勾，換來源或目標時清空，避免把
 * 上一組勾選帶到另一個角色）。plan 是 from→to 目前的計畫（key 記錄是哪一組的計畫）；每次重繪都重新讀取，寫入後計畫可能改變。
 */
const transfer = { from: "", to: "", picked: new Set<TransferCategory>() };
let transferPlan: { key: string; plans: CategoryPlan[] | null; error: string | null } | null = null;
let transferPlanSeq = 0;
/** renderCharacters() 時重建：計畫區與「轉移」按鈕，計畫非同步回來後就地更新。 */
let transferView: { box: HTMLElement; go: HTMLButtonElement } | null = null;

const charKey = (region: Region, id: string): string => `${region}:${id}`;

function parseCharKey(key: string): CharacterKey {
  const [region, id] = key.split(":");
  return { region: region as Region, id };
}

function findCharacter(key: string): CharacterView | null {
  if (!characters || !key) return null;
  const { region, id } = parseCharKey(key);
  return characters[region].characters.find((c) => c.id === id) ?? null;
}

const characterLabel = (c: CharacterView): string => c.name ?? `未命名（${c.id}）`;

function dateTime(sec: number): string {
  const d = new Date(sec * 1000);
  const pad = (n: number): string => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${hhmm(d)}`;
}

/** 備份 id（`YYYYMMDD-HHMMSS-mmm-<kind>`，本機時間）→「YYYY-MM-DD HH:MM:SS」。 */
function backupTime(id: string): string {
  return `${id.slice(0, 4)}-${id.slice(4, 6)}-${id.slice(6, 8)} ${id.slice(9, 11)}:${id.slice(11, 13)}:${id.slice(13, 15)}`;
}

export function renderCharacters(): void {
  const root = section("characters");
  if (!characters) {
    transferView = null;
    root.replaceChildren(h("p", { class: "hint" }, "無法讀取角色資料，原因見下方訊息列。"));
    return;
  }
  const list = characters;
  // 已不存在的角色（例如使用者刪了資料夾）：清掉選擇與展開狀態。
  if (!findCharacter(transfer.from)) transfer.from = "";
  if (!findCharacter(transfer.to)) transfer.to = "";
  const region = charRegion ?? draft?.region ?? "global";
  const tablist = subtabs({
    label: "區服",
    tabs: REGIONS,
    current: region,
    id: (r) => `char-tab-${r}`,
    controls: () => "char-panel",
    className: (r) => `subtab ${r}`,
    content: (r) => {
      const n = list[r].characters.length;
      return [regionLabel(r), h("span", { class: "subtab-count", title: `${n} 個角色` }, String(n))];
    },
    select: (r) => {
      charRegion = r;
      renderCharacters();
      document.getElementById(`char-tab-${r}`)?.focus();
    },
  });
  root.replaceChildren(
    transferBlock(list),
    tablist,
    h(
      "div",
      { id: "char-panel", role: "tabpanel", "aria-labelledby": `char-tab-${region}` },
      characterBlock(region, list[region]),
    ),
  );
  loadTransferPlan();
}

/** 來源／目標下拉：兩服角色依區服分組。 */
function characterSelect(id: string, label: string, list: CharacterList, value: string, onChange: (v: string) => void): HTMLSelectElement {
  const select = h("select", { id, "aria-label": label });
  select.append(option("", "選擇角色", value === ""));
  for (const r of REGIONS) {
    const chars = list[r].characters;
    if (!chars.length) continue;
    const group = h("optgroup", { label: regionLabel(r) });
    for (const c of chars) {
      const played = c.lastPlayed === null ? "" : `・${dateTime(c.lastPlayed)}`;
      group.append(option(charKey(r, c.id), `${characterLabel(c)}${played}`, value === charKey(r, c.id)));
    }
    select.append(group);
  }
  select.addEventListener("change", () => onChange(select.value));
  return select;
}

function transferBlock(list: CharacterList): HTMLElement {
  const from = characterSelect("transfer-from", "來源角色", list, transfer.from, (v) => {
    transfer.from = v;
    transfer.picked.clear();
    renderCharacters();
  });
  const to = characterSelect("transfer-to", "目標角色", list, transfer.to, (v) => {
    transfer.to = v;
    transfer.picked.clear();
    renderCharacters();
  });
  const box = h("div", { class: "transfer-plan", role: "group", "aria-label": "要轉移的設定" });
  const go = h("button", { id: "transfer-go", class: "primary", type: "button" }, "轉移");
  go.addEventListener("click", startTransfer);
  transferView = { box, go };
  return h(
    "section",
    { class: "settings-block" },
    h("h3", { class: "section-title" }, "轉移"),
    h(
      "p",
      { class: "hint" },
      "把來源角色的設定複製到目標角色，可跨區服。只轉下列分類；套裝、物品排序等綁角色自己的資料不轉。寫入前會自動備份目標角色。",
    ),
    h("div", { class: "transfer-pick" }, ...field("來源", from), ...field("目標", to)),
    box,
    h("div", { class: "toolbar" }, go),
  );
}

/** 讀取目前 from→to 的轉移計畫並更新計畫區；只讀檔案，不經 run()。 */
function loadTransferPlan(): void {
  const { from, to } = transfer;
  const key = `${from}>${to}`;
  const seq = ++transferPlanSeq;
  if (!from || !to || from === to) {
    transferPlan = null;
    paintTransferPlan();
    return;
  }
  // 同一組沿用上次的計畫先畫（避免重繪時閃成「讀取中」），回來後再更新。
  if (transferPlan?.key !== key) transferPlan = { key, plans: null, error: null };
  paintTransferPlan();
  api.planCharacterTransfer(parseCharKey(from), parseCharKey(to)).then(
    (plans) => {
      if (seq !== transferPlanSeq) return;
      transferPlan = { key, plans, error: null };
      paintTransferPlan();
    },
    (e: unknown) => {
      if (seq !== transferPlanSeq) return;
      transferPlan = { key, plans: null, error: errorText(e) };
      paintTransferPlan();
    },
  );
}

/** 轉移計畫中可轉、且使用者勾選的分類。 */
function transferChosen(): TransferCategory[] {
  return (transferPlan?.plans ?? []).filter((p) => p.action !== "skip" && transfer.picked.has(p.category)).map((p) => p.category);
}

function paintTransferPlan(): void {
  if (!transferView) return;
  const { box, go } = transferView;
  const syncGo = (): void => {
    go.disabled = runsActive > 0 || transferChosen().length === 0;
  };
  syncGo();
  if (!transfer.from || !transfer.to) {
    box.replaceChildren(h("p", { class: "field-hint" }, "選好來源與目標角色後，這裡列出可以轉移的設定。"));
    return;
  }
  if (transfer.from === transfer.to) {
    box.replaceChildren(h("p", { class: "field-error" }, "來源與目標是同一個角色。"));
    return;
  }
  if (!transferPlan || (transferPlan.plans === null && transferPlan.error === null)) {
    box.replaceChildren(h("p", { class: "field-hint" }, "讀取轉移計畫中…"));
    return;
  }
  if (transferPlan.error !== null) {
    box.replaceChildren(h("p", { class: "field-error" }, transferPlan.error));
    return;
  }
  box.replaceChildren(
    ...(transferPlan.plans ?? []).map((p) => {
      const id = `transfer-cat-${p.category}`;
      const check = h("input", { id, type: "checkbox" });
      check.disabled = p.action === "skip";
      check.checked = !check.disabled && transfer.picked.has(p.category);
      check.addEventListener("change", () => {
        if (check.checked) transfer.picked.add(p.category);
        else transfer.picked.delete(p.category);
        syncGo();
      });
      const note =
        p.action === "skip"
          ? `不轉：${p.reason ?? ""}`
          : p.action === "copy"
            ? "整檔複製"
            : `以 key 合併${p.missingKeys ? `；目標沒有的 ${p.missingKeys} 個 key 不會新增` : ""}`;
      return h(
        "div",
        { class: `transfer-row${p.action === "skip" ? " skip" : ""}` },
        check,
        h("label", { for: id }, TRANSFER_CATEGORY_TEXT[p.category]),
        h("span", { class: "field-hint" }, note),
      );
    }),
  );
}

function startTransfer(): void {
  const source = findCharacter(transfer.from);
  const target = findCharacter(transfer.to);
  const categories = transferChosen();
  if (!source || !target || !categories.length) return;
  const from = parseCharKey(transfer.from);
  const to = parseCharKey(transfer.to);
  const chosen = (transferPlan?.plans ?? []).filter((p) => categories.includes(p.category));
  const listOf = (action: CategoryPlan["action"]): string =>
    chosen
      .filter((p) => p.action === action)
      .map((p) => TRANSFER_CATEGORY_TEXT[p.category])
      .join("、");
  const copied = listOf("copy");
  const merged = listOf("merge");
  const ok = confirm(
    `把${regionLabel(from.region)}「${characterLabel(source)}」的設定轉移到${regionLabel(to.region)}「${characterLabel(target)}」？\n\n` +
      (copied ? `整檔覆寫：${copied}\n` : "") +
      (merged ? `合併：${merged}（只改目標已有的項目；目標獨有的保留，來源獨有的不加）\n` : "") +
      `\n寫入前會自動備份目標角色，可從它的備份清單還原。\n請先關閉${regionLabel(to.region)}的遊戲與官方啟動器。`,
  );
  if (!ok) return;
  void run("轉移角色設定", async () => {
    const r = await api.transferCharacter(from, to, categories);
    const done = r.done.map((c) => TRANSFER_CATEGORY_TEXT[c]).join("、");
    const skipped = r.skipped.map((p) => `${TRANSFER_CATEGORY_TEXT[p.category]}（${p.reason ?? ""}）`).join("、");
    if (!r.done.length) return { text: `沒有轉移任何設定：${skipped}`, kind: "error" };
    return skipped ? { text: `已轉移 ${done}；未轉移 ${skipped}`, kind: "" } : `已轉移 ${done}`;
  }).then(() => document.getElementById("transfer-go")?.focus());
}

function characterBlock(region: Region, data: RegionCharacters): HTMLElement {
  const label = regionLabel(region);
  const block = h("section", { class: "settings-block" }, h("h3", { class: "section-title" }, `${label}角色`));
  if (!data.exists) {
    block.append(
      h("p", { class: "hint" }, `找不到${label}的角色資料夾（${data.dir}）。在這台電腦用${label}登入過遊戲後才會出現。`),
    );
    return block;
  }
  if (!data.characters.length) {
    block.append(h("p", { class: "hint" }, `${data.dir} 裡沒有角色資料夾。在這台電腦用${label}登入過角色後才會出現。`));
    return block;
  }
  block.append(
    h("p", { class: "hint" }, "資料夾裡沒有角色名稱，請自己取名以便辨認。還原與轉移時需先關閉該區服的遊戲與官方啟動器。"),
    ...data.characters.map((c) => characterRow(region, c)),
  );
  return block;
}

function characterRow(region: Region, c: CharacterView): HTMLElement {
  const key = charKey(region, c.id);
  const usedBy = c.usage ? config.accounts.find((a) => a.id === c.usage?.accountId) : undefined;
  const meta = h(
    "span",
    { class: "meta" },
    h("span", {}, c.lastPlayed === null ? "沒有遊玩紀錄" : `最後遊玩 ${dateTime(c.lastPlayed)}`),
    // 帳號刪除後標記仍在，找不到帳號就不顯示。
    ...(usedBy ? [h("span", {}, `最後由 ${accountLabel(usedBy)} 使用`)] : []),
    ...(c.name === null ? [] : [h("span", {}, c.id)]),
  );
  const summary = h(
    "summary",
    {},
    h("span", { class: "card-main" }, h("span", { class: "title" }, characterLabel(c)), meta),
  );
  const details = h("details", { class: "character" }, summary);
  details.open = charOpen.has(key);
  details.addEventListener("toggle", () => {
    if (details.open) charOpen.add(key);
    else charOpen.delete(key);
  });
  details.append(characterBody(region, c, key));
  return details;
}

function characterBody(region: Region, c: CharacterView, key: string): HTMLElement {
  const saved = c.name ?? "";
  const nameId = `char-name-${region}-${c.id}`;
  const input = h("input", { id: nameId, value: charNameDrafts.get(key) ?? saved, placeholder: "例如：主號 白魔" });
  const save = h("button", { class: "secondary", type: "button" }, "儲存名稱");
  const syncSave = (): void => {
    save.disabled = input.value.trim() === saved;
  };
  syncSave();
  input.addEventListener("input", () => {
    if (input.value.trim() === saved) charNameDrafts.delete(key);
    else charNameDrafts.set(key, input.value);
    syncSave();
  });
  save.addEventListener("click", () => {
    const name = input.value.trim();
    void run(name ? "儲存角色名稱" : "清除角色名稱", async () => {
      await api.setCharacterLabel(region, c.id, name);
      charNameDrafts.delete(key);
    }).then(() => document.getElementById(nameId)?.focus());
  });

  const backupId = `char-backup-${region}-${c.id}`;
  const backup = h("button", { id: backupId, class: "secondary", type: "button" }, "備份");
  backup.addEventListener("click", () => {
    void run(`備份「${characterLabel(c)}」`, async () => {
      const id = await api.backupCharacter(region, c.id);
      return `已備份「${characterLabel(c)}」（${backupTime(id)}）`;
    }).then(() => document.getElementById(backupId)?.focus());
  });
  const actions: HTMLElement[] = [backup];
  // 備份資料夾不存在時開不起來（後端不建立），有備份才顯示。
  if (c.backups.length) {
    const open = h("button", { class: "ghost", type: "button" }, "開啟備份資料夾");
    open.addEventListener("click", () => openFolder({ kind: "characterBackups", region, id: c.id }));
    actions.push(open);
  }

  const backups = c.backups.length
    ? c.backups.map((b) => {
        const restore = h("button", { class: "ghost", type: "button" }, "還原");
        restore.addEventListener("click", () => {
          const ok = confirm(
            `把「${characterLabel(c)}」還原成 ${backupTime(b.id)} 的備份？\n\n還原前會自動備份目前的檔案。請先關閉${regionLabel(region)}的遊戲與官方啟動器。`,
          );
          if (!ok) return;
          void run(`還原「${characterLabel(c)}」`, async () => {
            await api.restoreCharacter(region, c.id, b.id);
            return `已把「${characterLabel(c)}」還原成 ${backupTime(b.id)} 的備份；還原前的檔案已自動備份`;
          }).then(() => document.getElementById(backupId)?.focus());
        });
        return h(
          "li",
          { class: "backup-row" },
          h("span", {}, backupTime(b.id)),
          h("span", { class: "badge" }, b.kind === "manual" ? "手動" : "自動"),
          restore,
        );
      })
    : [h("li", { class: "field-hint" }, "還沒有備份。")];

  return h(
    "div",
    { class: "character-body" },
    h("div", { class: "character-name" }, ...field("名稱", input), save),
    h("div", { class: "toolbar" }, ...actions),
    h("p", { class: "field-hint" }, "自動備份在還原與轉移前建立，每個角色只留最新 10 份；手動備份不會自動刪除。"),
    h("ul", { class: "backup-list", "aria-label": "備份" }, ...backups),
  );
}
