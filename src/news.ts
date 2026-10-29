import { api, regionLabel, type GlobalLanguage, type Region, type NewsItem } from "./api";
import { errorText, h, say } from "./app";
import { draft, type LoginDraft } from "./login";

// ---------- 官方消息 ----------

/**
 * 側欄的官方消息（protocols §1.4.2、§2.7）：跟登入面板目前的區服（新增帳號時跟表單選的區服），國際服再依「進階 › 語言」
 * 取該語言的公告。各服（國際服各語言）各自快取：第一次用到才抓，之後換帳號不重抓、也不定時重抓；取不到時可點擊重試，
 * 只重抓那一份，原因放在重試按鈕的 title。
 */
type NewsState = { kind: "loading" } | { kind: "error"; reason: string } | { kind: "loaded"; items: NewsItem[] };
/** 要顯示的那一份消息；key 是快取鍵（國際服含語言）。 */
interface NewsSource {
  region: Region;
  language: GlobalLanguage;
  key: string;
}
const newsCache = new Map<string, NewsState>();
/** accountPane() 新建的面板；抓取結果回來，或區服、語言改了時就地更新，不重繪整個登入頁。 */
let newsPanel: { root: HTMLElement; title: HTMLElement; shop: HTMLButtonElement; body: HTMLElement } | null = null;

const SHOP_TITLE: Record<Region, string> = {
  global: "以瀏覽器開啟國際服 Mog Station（水晶商城）",
  tw: "以瀏覽器開啟繁中服水晶商城",
};

function newsSource(d: LoginDraft): NewsSource {
  return { region: d.region, language: d.language, key: d.region === "global" ? `global-${d.language}` : "tw" };
}

function loadNews(src: NewsSource): void {
  newsCache.set(src.key, { kind: "loading" });
  paintNews();
  const fetching = src.region === "global" ? api.getGlobalNews(src.language) : api.getTwNews();
  void fetching.then(
    (items) => {
      newsCache.set(src.key, { kind: "loaded", items });
      paintNews();
    },
    (e: unknown) => {
      newsCache.set(src.key, { kind: "error", reason: errorText(e) });
      paintNews();
    },
  );
}

/** 開消息不走 run()：不鎖按鈕、不重繪，失敗只顯示訊息（同 openFolder）。 */
function openNews(region: Region, url: string): void {
  api.openNews(region, url).catch((e: unknown) => say(errorText(e), "error"));
}

/** 依登入面板目前的區服與語言重畫消息面板；那一份還沒抓過就開始抓。 */
export function paintNews(): void {
  if (!newsPanel || !draft) return;
  const src = newsSource(draft);
  const s = newsCache.get(src.key);
  if (!s) {
    loadNews(src);
    return;
  }
  const { root, title, shop, body } = newsPanel;
  root.className = `news-group ${src.region}`;
  title.textContent = `${regionLabel(src.region)}消息`;
  shop.title = SHOP_TITLE[src.region];
  switch (s.kind) {
    case "loading":
      body.replaceChildren(h("p", { class: "news-note" }, "載入中…"));
      return;
    case "error": {
      const retry = h("button", { type: "button", class: "news-note news-retry", title: s.reason }, "無法取得消息，點此重試");
      retry.addEventListener("click", () => loadNews(src));
      body.replaceChildren(retry);
      return;
    }
    case "loaded":
      body.replaceChildren(
        s.items.length
          ? h("ul", { class: "news-list" }, ...s.items.map((n) => newsItem(src.region, n)))
          : h("p", { class: "news-note" }, "目前沒有消息"),
      );
  }
}

/** 一則消息：標題（最多兩行，超出省略，完整標題在 title）與日期各一行；點擊以系統瀏覽器開啟。 */
function newsItem(region: Region, n: NewsItem): HTMLElement {
  const b = h(
    "button",
    { type: "button", class: "news-item", title: n.title },
    h("span", { class: "news-item-title" }, n.title),
    h("span", { class: "news-item-date" }, n.date),
  );
  b.addEventListener("click", () => openNews(region, n.url));
  return h("li", {}, b);
}

/**
 * 側欄帳號清單下方的消息面板：區服色底與細條，標題列左邊是圖示＋「國際服消息」／「繁中服消息」（區服色），右邊是「水晶商城」
 * （protocols §1.4.3、§2.8；全程式唯一的商城入口），開同一個區服的官方商城。
 */
export function newsGroup(): HTMLElement {
  const title = h("span");
  const shop = h(
    "button",
    { type: "button", class: "secondary shop-button" },
    h("span", { class: "shop-icon", "aria-hidden": "true" }),
    "水晶商城",
  );
  // 同開消息：不走 run()，失敗只顯示訊息。跟著面板目前的區服（draft 換區服時 paintNews 會更新標題與 title）。
  shop.addEventListener("click", () => {
    if (draft) api.openShop(draft.region).catch((e: unknown) => say(errorText(e), "error"));
  });
  const body = h("div", { class: "news-body" });
  const root = h(
    "section",
    { class: "news-group", "aria-labelledby": "news-title" },
    h(
      "div",
      { class: "news-head" },
      h("h3", { class: "news-title", id: "news-title" }, h("span", { class: "news-icon", "aria-hidden": "true" }), title),
      shop,
    ),
    body,
  );
  newsPanel = { root, title, shop, body };
  paintNews();
  return root;
}
