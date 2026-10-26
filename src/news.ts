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
let newsPanel: { root: HTMLElement; title: HTMLElement; body: HTMLElement } | null = null;

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
  const { root, title, body } = newsPanel;
  root.className = `news-group ${src.region}`;
  title.textContent = `${regionLabel(src.region)}消息`;
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

/** 側欄帳號清單下方的消息面板：區服色底與細條，標題（圖示＋「國際服消息」／「繁中服消息」）用區服色。 */
export function newsGroup(): HTMLElement {
  const title = h("span");
  const body = h("div", { class: "news-body" });
  const root = h(
    "section",
    { class: "news-group", "aria-labelledby": "news-title" },
    h("h3", { class: "news-title", id: "news-title" }, h("span", { class: "news-icon", "aria-hidden": "true" }), title),
    body,
  );
  newsPanel = { root, title, body };
  paintNews();
  return root;
}
