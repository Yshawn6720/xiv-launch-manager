import { h } from "./app";

// 與 Rust AppError::DalamudOlderThanGame 同步；其他登入失敗不能進確認分支。
export const TW_DALAMUD_UNCONFIRMED_PREFIX = "繁中服 Dalamud 相容性尚未確認：";

type Choice = "load" | "plain" | "cancel";

export function confirmTwDalamud(details: string): Promise<Choice> {
  const cancel = h("button", { class: "ghost", type: "submit", value: "cancel" }, "取消");
  cancel.autofocus = true;
  const dialog = h(
    "dialog",
    {
      class: "tw-dalamud-confirmation",
      "aria-labelledby": "tw-dalamud-confirmation-title",
      "aria-describedby": "tw-dalamud-confirmation-details",
    },
    h(
      "form",
      { method: "dialog" },
      h("h2", { id: "tw-dalamud-confirmation-title" }, "這個 Dalamud 版本尚未確認相容"),
      h("p", { id: "tw-dalamud-confirmation-details" }, `${details}。繼續載入可能造成遊戲崩潰或插件異常。這次選擇不會修改帳號設定。`),
      h(
        "div",
        { class: "actions" },
        cancel,
        h("button", { class: "secondary", type: "submit", value: "load" }, "繼續載入 Dalamud"),
        h("button", { class: "primary", type: "submit", value: "plain" }, "不載入 Dalamud，直接登入"),
      ),
    ),
  );
  document.body.append(dialog);
  return new Promise((resolve) => {
    dialog.addEventListener("close", () => {
      const choice = dialog.returnValue;
      dialog.remove();
      resolve(choice === "load" || choice === "plain" ? choice : "cancel");
    }, { once: true });
    dialog.showModal();
  });
}
