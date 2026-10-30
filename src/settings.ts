import { api, regionLabel, type AfterLaunch, type LicenseDoc, type Theme } from "./api";
import { applyTheme, config, errorText, h, openFolder, REGIONS, run, say, section } from "./app";
import { render, showTab } from "./main";

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
      "本程式以一般使用者權限執行，不要求系統管理員。國際服遊戲更新只在安裝資料夾受保護（一般權限不能修改，例如裝在 Program Files）時才跳 UAC：同意後執行的是本程式自己的更新 helper，只負責核對與套用更新檔、寫入版本檔，不開視窗，完成即結束；helper 發現安裝資料夾一般權限就能修改時不寫入。以一般身分執行、且國際服安裝資料夾本程式就能修改時，以及繁中服，更新都在本程式內套用，不跳 UAC；以系統管理員身分執行本程式時，國際服更新一律交給 helper（不跳 UAC），一般權限就能修改的安裝不會更新。",
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
export function showSafety(): void {
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

/** 開啟國際服外部包前的風險說明（只問一次，確認後記在設定）。 */
const EXTERNAL_PACK_RISK =
  "國際服外部包是對遊戲用戶端的非官方修改，違反 Square Enix 使用者協議，可能導致帳號被處分。\n\n" +
  "是否使用、以及因此造成的帳號處分或其他損失，由你自行承擔。確認後不再詢問。\n\n" +
  "確定要開啟？";

/** 「國際服外部包」的說明段落：粗體開頭＋內文。 */
function packNote(lead: string, text: string): HTMLElement {
  return h("p", { class: "hint" }, h("strong", {}, lead), text);
}

/**
 * 「國際服外部包」（protocols §1.6）：功用一句、資料夾列（同遊戲路徑的輸入＋選擇資料夾＋儲存）、開關，以及收起的「說明」
 * （放什麼、怎麼運作、限制與風險）。「選擇資料夾」同「自動偵測」只填入輸入框，仍要按儲存。
 * 開關以輸入框目前的資料夾送出；第一次開啟先確認風險，不同意就不送出、開關退回關閉。失敗由 run() 顯示原因並重繪，
 * 開關回到已存的狀態。
 */
function externalPackBlock(): HTMLElement {
  const pack = config.externalPack;
  const saved = pack.dir ?? "";
  const input = h("input", {
    value: saved,
    "aria-label": "國際服外部包資料夾",
    placeholder: "例：D:\\FFXIV 外部包",
    autocomplete: "off",
    spellcheck: "false",
  });
  const pick = h("button", { class: "secondary", type: "button" }, "選擇資料夾");
  const save = h("button", { class: "primary" }, "儲存");
  const toggleId = "settings-external-pack";
  const toggle = h("input", { id: toggleId, type: "checkbox" });
  toggle.checked = pack.enabled;
  // 沒填資料夾時不能開啟；已開啟時照樣可以關（資料夾被移走也能關）。
  const sync = () => {
    const dir = input.value.trim();
    save.disabled = dir === saved;
    toggle.disabled = !pack.enabled && dir === "";
  };
  sync();
  input.addEventListener("input", sync);
  // 不走 run()：不重繪（輸入框裡還沒儲存的內容會不見），失敗只顯示訊息（同 openFolder）。對話框開著時按鈕停用，免得連點開出第二個。
  pick.addEventListener("click", () => {
    pick.disabled = true;
    api
      .pickFolder("選擇國際服外部包資料夾", input.value.trim() || null)
      .then(
        (dir) => {
          if (dir === null) return;
          input.value = dir;
          sync();
          save.focus();
        },
        (e: unknown) => say(errorText(e), "error"),
      )
      .finally(() => {
        pick.disabled = false;
      });
  });
  save.addEventListener("click", () =>
    run("儲存國際服外部包資料夾", () => api.setExternalPack(input.value.trim() || null, pack.enabled, false)),
  );
  toggle.addEventListener("change", () => {
    const on = toggle.checked;
    const acceptRisk = on && !pack.riskAccepted;
    if (acceptRisk && !confirm(EXTERNAL_PACK_RISK)) {
      toggle.checked = false;
      return;
    }
    void run(on ? "開啟國際服外部包" : "關閉國際服外部包", async () => {
      await api.setExternalPack(input.value.trim() || null, on, acceptRisk);
      return on ? "已開啟國際服外部包，下次登入國際服時套用" : "已關閉國際服外部包，國際服改以原版遊戲啟動";
    }).then(() => document.getElementById(toggleId)?.focus());
  });
  // 開瀏覽器不走 run()：不重繪，失敗只顯示訊息（同 openFolder）。
  const demo = h("button", { class: "link", type: "button" }, "看示範影片");
  demo.addEventListener("click", () => {
    api.openPackDemo().catch((e: unknown) => say(errorText(e), "error"));
  });
  const guide = h(
    "details",
    { class: "editor-advanced pack-guide" },
    h("summary", {}, "說明：資料夾要放什麼、怎麼運作"),
    h(
      "div",
      { class: "editor-advanced-body" },
      h("p", { class: "hint" }, demo, "：在瀏覽器開啟使用說明，影片約 30 秒，示範從選資料夾到開啟的步驟。"),
      packNote(
        "要放什麼：",
        "把要替換的 sqpack 檔直接放在這個資料夾（不用子資料夾），檔名和遊戲 game\\sqpack\\ffxiv\\ 裡的官方檔相同，例如字體包 000000.win32.dat0、.index、.index2，漢化包 0a0000.win32.dat0、.index、.index2。每組至少要有 .dat0 和 .index，其他檔案不理會。",
      ),
      packNote(
        "怎麼運作：",
        "開啟後登入國際服時，本程式在遊戲資料夾旁建一個「影子資料夾」（xiv-launch-manager-shadow），以連結組合官方檔與外部包，再從那裡啟動遊戲。不另佔遊戲大小的空間；遊戲更新、檢查遊戲檔完整性照常。",
      ),
      packNote(
        "重建與 UAC：",
        "登入時檢查影子資料夾，第一次使用或檔案有變時才重建；需要重建且遊戲裝在 Program Files 等受保護位置時，Windows 才會詢問是否允許（UAC），按「否」這次就以原版遊戲啟動。已有影子的遊戲在執行時沿用，不重建。",
      ),
      packNote(
        "遊戲更新後：",
        "外部包是對某一版官方檔做的。版本不符時這次以原版遊戲啟動並提示「外部包已過期」，請用新版官方檔重新產生外部包。",
      ),
      packNote(
        "限制：",
        "外部包資料夾要和遊戲在同一個磁碟，且不能與遊戲資料夾或影子資料夾重疊（不能放在裡面，也不能包含它們）。不要用其他工具修改影子資料夾裡的檔案：它們連到遊戲原檔或外部包，修改會直接影響來源。關閉後影子資料夾會留著；要刪請用檔案總管，不要用會跟著連結刪除內容的工具。",
      ),
      packNote("風險：", "非官方修改違反 Square Enix 使用者協議，可能導致帳號被處分；第一次開啟時會再請你確認。"),
    ),
  );
  return h(
    "section",
    { class: "settings-block" },
    h("h3", { class: "section-title" }, "國際服外部包"),
    h(
      "p",
      { class: "hint" },
      "讓國際服用上字體包、漢化包（例如 GP 漢化器產生的檔案），遊戲資料夾裡的官方檔案不必改動。",
    ),
    // 與上方「遊戲路徑」同一種列：區服 badge＋輸入框＋按鈕＋儲存（「選擇資料夾」在「自動偵測」的位置）。
    h(
      "div",
      { class: "setting-row path-setting" },
      h("span", { class: "badge global" }, regionLabel("global")),
      input,
      pick,
      save,
    ),
    h("div", { class: "setting-row" }, h("label", { class: "check" }, toggle, "登入國際服時套用外部包")),
    guide,
  );
}

export function renderSettings(): void {
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
    externalPackBlock(),
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
