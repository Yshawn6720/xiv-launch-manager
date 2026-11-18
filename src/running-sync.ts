import { DALAMUD_GAME_VERSION_MISMATCH_PREFIX, type Region, type RunningGame } from "./api.ts";

/** 先讓國際服登入判斷遊戲是否需要更新；注入前仍會核對版本。 */
export function canContinueAfterDalamudPrepareError(region: Region, message: string): boolean {
  return region === "global" && message.startsWith(DALAMUD_GAME_VERSION_MISMATCH_PREFIX);
}

const earlyExits = new Map<number, string>();

export function rememberEarlyExit(pid: number, message: string): void {
  earlyExits.set(pid, message);
}

export function takeEarlyExit(pid: number): string | undefined {
  const message = earlyExits.get(pid);
  earlyExits.delete(pid);
  return message;
}

export function clearEarlyExits(): void {
  earlyExits.clear();
}

// 遊戲正常關閉沒有事件（只有提早結束會通知），畫面上的「執行中」只能靠輪詢結果撤銷。這裡只算要改什麼，不碰 DOM。

/** 登入成功後訊息列顯示的啟動結果（含 pid）；該遊戲結束後要改寫，不能一直寫著「已啟動」。 */
export interface LaunchNotice {
  accountId: string;
  pid: number;
  label: string;
  /** 訊息列上的完整文字；訊息列已換成別的內容就不動。 */
  text: string;
}

export interface RunningSync {
  /** 側欄要標「遊戲執行中」的帳號 id。 */
  accountsRunning: string[];
  /** Dalamud 分頁要標「使用中」（並停用「複製」）的設定檔 id。 */
  profilesInUse: string[];
  /** 訊息列要改成的文字；null＝不動。 */
  message: string | null;
}

/**
 * 把一次輪詢結果（本程式啟動且仍在執行的遊戲、仍被使用的設定檔）對到畫面；`shownMessage` 是訊息列目前的文字。
 * 啟動訊息的遊戲以帳號與 pid 一起比對：同帳號重開算舊的那個已結束。
 */
export function syncRunning(
  notice: LaunchNotice | null,
  shownMessage: string,
  games: readonly RunningGame[],
  profilesInUse: readonly string[],
): RunningSync {
  const ended =
    notice !== null &&
    shownMessage === notice.text &&
    !games.some((g) => g.accountId === notice.accountId && g.pid === notice.pid);
  return {
    accountsRunning: games.map((g) => g.accountId),
    profilesInUse: [...profilesInUse],
    message: ended ? `${notice.label} 的遊戲已結束（pid ${notice.pid}）` : null,
  };
}
