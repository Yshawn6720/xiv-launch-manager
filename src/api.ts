// 與 src-tauri/src/commands.rs 的 serde 形狀一一對應（camelCase、tagged enum）。
import { Channel, invoke } from "@tauri-apps/api/core";

export type Region = "global" | "tw";
export type GlobalLanguage = "japanese" | "english" | "german" | "french";
export type OtpMode = "none" | "manual" | "auto";

export type YanmucorpVersion = { kind: "latest" } | { kind: "pinned"; tag: string };
export type DalamudChannel =
  | { source: "goatcorp" }
  | { source: "yanmucorp"; version: YanmucorpVersion };

export interface Profile {
  id: string;
  name: string;
  channel: DalamudChannel;
  injectionDelayMs: number;
  /** 沿用的舊啟動器 Dalamud 資料夾；null＝本程式管理的 profiles/<id>/。 */
  dataDir: string | null;
  /** 已確認可與執行中的遊戲共用；確認後同時使用不再詢問（protocols §3）。 */
  shareConfirmed: boolean;
}

export interface DetectedDalamudData {
  label: string;
  region: Region;
  path: string;
  pluginCount: number;
}

export interface AccountView {
  id: string;
  name: string;
  region: Region;
  username: string;
  otp: OtpMode;
  profileId: string | null;
  language: GlobalLanguage;
  hasPassword: boolean;
  hasOtpSecret: boolean;
  /** false＝不儲存密碼，每次登入時輸入。 */
  rememberPassword: boolean;
  /** 額外遊戲參數：空白分隔的 key=value。 */
  extraArgs: string;
  /** 「ACT 跟這個帳號」：登入時寫 ACT 連動指令檔並依區服的 ACT 設定自動開啟；舊設定檔沒有此欄位時後端給 true。 */
  actFollow: boolean;
}

/** Dalamud 設定檔的磁碟狀態（後端依實際檔案推算）。 */
export interface ProfileInfo {
  id: string;
  pluginCount: number;
  /** 依通道推算目前會用到的已安裝版本（安裝目錄名）；null＝尚未下載。 */
  installedVersion: string | null;
  /** installedVersion 那個安裝目錄的 `Dalamud.dll` 檔案版本；尚未下載或讀不到為 null。 */
  installedDalamudVersion: string | null;
}

/** `duplicate_profile` 的結果。 */
export interface DuplicatedProfile {
  id: string;
  name: string;
  /** 略過、沒有複製的 symlink／junction 數。 */
  skippedLinks: number;
}

export interface ConfigView {
  games: { global: string | null; tw: string | null };
  accounts: AccountView[];
  profiles: Profile[];
  profilesInUse: string[];
  dataDir: string;
  profileInfo: ProfileInfo[];
  /** 已下載的 Dalamud 本體（與 `list_installed_dalamud` 同一份）。 */
  installed: InstalledVersion[];
  /** 遊戲仍在執行中的帳號 id。 */
  accountsRunning: string[];
  ui: UiSettings;
  externalPack: ExternalPack;
}

/**
 * 國際服外部包（Rust `ExternalPack`，protocols §1.6）：字體包、漢化包所在的資料夾。開啟時國際服從影子資料夾啟動，
 * 安裝資料夾維持官方原檔。riskAccepted＝使用者已確認過風險（只確認一次）。
 */
export interface ExternalPack {
  dir: string | null;
  enabled: boolean;
  riskAccepted: boolean;
}

/** 設定 › 介面 › 主題（Rust `Theme`）：system＝跟隨系統；設定檔沒有這一欄時是 dark。 */
export type Theme = "system" | "dark" | "light";

/** 設定 › 介面 › 遊戲啟動後（Rust `AfterLaunch`）：keep＝不動作；tray＝縮到系統匣。 */
export type AfterLaunch = "keep" | "minimize" | "tray";

/** 設定分頁「介面」（Rust `UiSettings`）。 */
export interface UiSettings {
  theme: Theme;
  afterLaunch: AfterLaunch;
}

export type ServerState = "open" | "maintenance" | "unknown";

/** 兩服伺服器狀態：國際服＝登入閘道狀態；台服＝官網 World 狀態頁（只用於顯示）。 */
export type ServerStatus = Record<Region, ServerState>;

/** 一則官方消息（Rust `NewsItem`，protocols §1.4.2、§2.7）；date 兩服同為 YYYY/MM/DD（國際服是 UTC 日期），url 已通過該服白名單。 */
export interface NewsItem {
  title: string;
  date: string;
  url: string;
}

/** 開資料夾的目標；路徑一律由後端依設定解析。 */
export type FolderTarget =
  | { kind: "dataRoot" }
  | { kind: "profile"; id: string }
  | { kind: "profileLogs"; id: string }
  | { kind: "detected"; path: string }
  /** 該區服的遊戲設定資料夾（文件\My Games\ 下）。 */
  | { kind: "gameConfig"; region: Region }
  /** 角色的備份資料夾（%APPDATA%\xiv-launch-manager\characters\ 下）。 */
  | { kind: "characterBackups"; region: Region; id: string };

/** 角色備份的種類（Rust `BackupKind`）：auto＝還原或轉移前自動建立，每角色只留最新 10 份。 */
export type BackupKind = "manual" | "auto";

/** 一份角色備份；id＝`YYYYMMDD-HHMMSS-mmm-<kind>`（本機時間），字串順序即時間順序。 */
export interface CharacterBackup {
  id: string;
  kind: BackupKind;
}

/** 「最後由 <帳號> 使用」標記（Rust `Usage`）；at 為 Unix 秒。 */
export interface CharacterUsage {
  accountId: string;
  at: number;
}

/** 一個角色（Rust `CharacterView`）：id＝16 位 hex；lastPlayed 為 Unix 秒；backups 新到舊。 */
export interface CharacterView {
  id: string;
  lastPlayed: number | null;
  backups: CharacterBackup[];
  name: string | null;
  usage: CharacterUsage | null;
}

/** 一個區服的角色（Rust `RegionCharacters`）；exists 為 false＝區服資料夾不存在（遊戲沒在這台電腦啟動過）。 */
export interface RegionCharacters {
  dir: string;
  exists: boolean;
  characters: CharacterView[];
}

export type CharacterList = Record<Region, RegionCharacters>;

/** 一個角色的識別（Rust `CharacterKey`）。 */
export interface CharacterKey {
  region: Region;
  id: string;
}

/** 轉移分類（Rust `Category`，protocols §7.3）。 */
export type TransferCategory =
  | "hotbar"
  | "macro"
  | "keybind"
  | "chatFilter"
  | "hud"
  | "characterSettings"
  | "controls";

/** 一個分類的轉移計畫（Rust `CategoryPlan`）；skip 時 reason 說明原因；missingKeys＝合併時目標沒有、不會新增的 key 數。 */
export interface CategoryPlan {
  category: TransferCategory;
  action: "copy" | "merge" | "skip";
  reason: string | null;
  missingKeys: number;
}

/** 轉移結果（Rust `TransferResult`）；backup＝寫入前的自動備份 id，沒有寫入時為 null。 */
export interface TransferResult {
  done: TransferCategory[];
  skipped: CategoryPlan[];
  backup: string | null;
}

/** 帳號要套用的 Dalamud 設定檔；detected＝偵測到的舊資料路徑（後端沿用對應的設定檔，沒有就在同一次寫入中新增後綁定）。 */
export type BindTarget =
  | { kind: "none" }
  | { kind: "profile"; id: string }
  | { kind: "detected"; path: string };

export interface AccountInput {
  id: string;
  name: string;
  region: Region;
  username: string;
  otp: OtpMode;
  profileId: string | null;
  language: GlobalLanguage;
  /** 非空才更新已存的密碼（rememberPassword 為 true 時）。 */
  password: string | null;
  otpSecret: string | null;
  /** true 且 otpSecret 為空＝刪除已存的 OTP 金鑰。 */
  clearOtpSecret: boolean;
  /** false＝刪除已存的密碼且不再儲存。 */
  rememberPassword: boolean;
  extraArgs: string;
  actFollow: boolean;
}

/** 本次載入哪些 plugin。 */
export type PluginLoad = "all" | "noThirdParty" | "none";

/** 本次是否比對衛月支援的遊戲版本（只有國際服 goatcorp 有這項比對）；skip＝使用者本次選了「強制載入」。 */
export type GameVersionCheck = "enforce" | "skip";

export interface ReleaseInfo {
  tag: string;
  publishedAt: string;
}

export interface InstalledVersion {
  source: string;
  /** 安裝目錄名：國際服為 AssemblyVersion，台服為 release tag（yy-mm-dd-nn）。 */
  version: string;
  /** 該目錄下 `Dalamud.dll` 的檔案版本（a.b.c.d）；讀不到為 null。 */
  dalamudVersion: string | null;
}

export interface LaunchOutcome {
  pid: number;
  /** 這次載入的 Dalamud 安裝目錄名（同 `InstalledVersion.version`）；沒載入為 null。 */
  dalamudVersion: string | null;
  /** 遊戲已啟動，但「登入遊戲後自動開啟 ACT」沒能開起 ACT 的原因（完整句子）；沒開或已開起為 null。 */
  actWarning: string | null;
  /** 遊戲已啟動，但 ACT 連動指令檔沒寫成的原因（完整句子）；寫成為 null。 */
  actTargetWarning: string | null;
  /** 國際服外部包已開啟，但這次以原版遊戲啟動的原因（完整句子；過期、讀不到、沒同意 UAC 等）；有套用或沒開啟為 null。 */
  packWarning: string | null;
  /** 繁中服這次登入時的剩餘遊戲時間文字（例「剩餘約 30 天」，取不到是「無法取得」）；國際服為 null。 */
  playTime: string | null;
}

/** 一組 ACT（Advanced Combat Tracker）設定。 */
export interface ActSettings {
  /** ACT 執行檔的完整路徑；null＝尚未設定。 */
  path: string | null;
  /** 登入該區服的遊戲後自動開啟 ACT。 */
  launchWithGame: boolean;
}

/** ACT 設定（Rust `ActConfig`）：國際服、繁中服各一組；兩組可指向同一個執行檔（單開）。 */
export type ActConfig = Record<Region, ActSettings>;

/** ACT 狀態（Rust `ActStatus`）：notConfigured＝尚未設定路徑；missing＝設定的執行檔已不存在。 */
export type ActStatus = "notConfigured" | "missing" | "running" | "stopped";

/** 各區服 ACT 的狀態（Rust `ActStatusByRegion`）；兩組指向同一個執行檔時兩區狀態相同。 */
export type ActStatusByRegion = Record<Region, ActStatus>;

/** 本程式啟動、仍在執行的一個遊戲（Rust `RunningGame`）。 */
export interface RunningGame {
  accountId: string;
  pid: number;
}

/** 頂端狀態列的查詢結果（Rust `RunningStatus`）；games 依帳號 id 排序。 */
export interface RunningStatus {
  act: ActStatusByRegion;
  games: RunningGame[];
  /** 正被仍在執行的遊戲使用的 Dalamud 設定檔 id（依 id 排序）。 */
  profilesInUse: string[];
}

/** `launch_act` 的結果：已在執行就不再開。 */
export type ActLaunch = "started" | "alreadyRunning";

/**
 * ACT 下拉選單的一項（Rust `ActChoice`）：installed＝系統安裝的 ACT；dual＝本程式建立的雙開 ACT（附服務的區服）；
 * custom＝設定裡已存、不是前兩種的路徑。同一個執行檔只列一次（不分大小寫）。
 */
export type ActChoice = {
  path: string;
  /** 執行檔存在；系統安裝與雙開一定存在，自訂路徑可能已被移走。 */
  exists: boolean;
  /** 目前設定選的就是這一項的區服。 */
  selectedBy: Region[];
} & ({ kind: "installed" | "custom"; region: null } | { kind: "dual"; region: Region });

/**
 * 連動外掛在一個 ACT 設定檔裡的狀態（Rust `LinkState`）：exeMissing＝執行檔不見了；neverOpened＝這個 ACT
 * 還沒開過（沒有設定檔）；unreadable＝設定檔讀不懂；disabled＝有登記但在 ACT 裡被取消勾選。
 */
export type ActLinkState = "exeMissing" | "neverOpened" | "unreadable" | "notRegistered" | "disabled" | "registered";

/** 一個已設定的 ACT（Rust `ActLink`）；兩組設定指向同一個執行檔時合成一筆。 */
export interface ActLink {
  /** 用這個 ACT 的區服（國際服在前）。 */
  regions: Region[];
  exe: string;
  state: ActLinkState;
  /** 本程式建立的雙開 ACT。 */
  instance: boolean;
}

/** 本程式建立、目前存在的雙開 ACT（Rust `Instance`）。 */
export interface ActInstance {
  region: Region;
  exe: string;
  /** 該區服的 ACT 設定目前就是它。 */
  inUse: boolean;
}

/** ACT 連動狀態（Rust `LinkStatus`）。 */
export interface ActLinkStatus {
  /** 已設定的 ACT（國際服在前）；都沒設定時為空。 */
  acts: ActLink[];
  /** 存在的雙開 ACT（國際服在前）。 */
  instances: ActInstance[];
}

/**
 * 一個區服 ACT 的 FFXIV 插件（Rust `PluginStatus`）：notConfigured＝該區服沒設定 ACT；exeMissing＝ACT 執行檔不見了；
 * neverOpened＝ACT 還沒開過（沒有設定檔）；unreadable＝設定檔讀不懂；notInstalled＝ACT 沒有登記 FFXIV 插件或檔案不在；
 * versionUnknown＝插件檔讀不到版本；installed 的 outdated＝GitHub 有比它新的版本（查不到最新版時為 false）。
 */
export type ActPluginStatus =
  | { state: "notConfigured" | "exeMissing" | "neverOpened" | "unreadable" | "notInstalled" | "versionUnknown" }
  | { state: "installed"; version: string; outdated: boolean };

/** 各區服 ACT 的 FFXIV 插件版本（Rust `PluginVersions`）；兩服用同一個 ACT 時狀態相同。 */
export interface ActPluginVersions {
  global: ActPluginStatus;
  tw: ActPluginStatus;
  /** GitHub 上 ravahn/FFXIV_ACT_Plugin 的最新 release；查詢失敗為 null。 */
  latest: string | null;
}

/** 一個 ACT 設定檔的登記結果（Rust `RegisterChange`）。 */
export type ActRegisterChange = "added" | "reenabled" | "unchanged";

/** 「啟用 ACT 連動」對一個 ACT 做了什麼（Rust `ActChange`）。 */
export interface ActChange {
  regions: Region[];
  exe: string;
  change: ActRegisterChange;
}

/** 「建立另一服的 ACT」的結果（Rust `DualResult`）；ACT 設定已依此寫回。 */
export interface ActDualResult {
  /** 主 ACT 的區服（它的 FFXIV 插件 Region）。 */
  mainRegion: Region;
  mainExe: string;
  instanceRegion: Region;
  instanceExe: string;
  /** false＝資料夾原本就在，沒有重建。 */
  created: boolean;
}

export interface OtpPreview {
  code: string;
  /** 這組 OTP 還剩幾秒有效（1–30）。 */
  secondsLeft: number;
}

/** 下載／安裝／核對的進度（Rust `dalamud::Progress`）；total 未知時為 null。 */
export interface Progress {
  stage: string;
  done: number;
  total: number | null;
}

export interface ProgressPayload extends Progress {
  accountId: string;
}

export const PROGRESS_EVENT = "launch-progress";

/**
 * 登入已回成功、背景觀察到遊戲提早結束（可能超過客戶端實例上限）時的事件（Rust `launch::GAME_EXITED_EARLY_EVENT`）。
 * 後端已撤銷該次的執行狀態並叫回主視窗；message 與同步回錯時的錯誤訊息相同。
 */
export const GAME_EXITED_EARLY_EVENT = "game-exited-early";

export interface GameExitedEarlyPayload {
  accountId: string;
  message: string;
}

/** `install_dalamud` 的進度事件 accountId。 */
export const installProgressId = (r: Region): string => `install:${r}`;

/**
 * 使用者取消時後端回的錯誤訊息：下載被 `cancel_dalamud_download` 取消（`AppError::Cancelled`，Dalamud、遊戲更新與
 * 完整性檢查共用），或在繁中服登入確認視窗按「取消」／關窗（`AppError::RecaptchaCancelled`）。不算登入失敗。
 */
export const CANCELLED_MESSAGE = "已取消";

/**
 * 啟動時版本檢查取得了 patch list（`AppError::UpdateRequired`，清單已存成 pending）時錯誤訊息的結尾；
 * 登入失敗訊息以此結尾＝需要更新，前端接著自動開始遊戲更新。與 Rust 訊息同步。
 */
export const UPDATE_REQUIRED_SUFFIX = "，請先執行遊戲更新";

/** `install_app_update` 的進度（Rust `UpdateProgress`）：下載中，或已通過驗簽、正在安裝（安裝版啟動安裝程式，免安裝版替換 exe 並啟動新版）。 */
export type UpdateProgress = { kind: "download"; done: number; total: number | null } | { kind: "install" };

/** 「設定 › 免責與安全」可閱讀的授權文件（Rust `LicenseDoc`）。 */
export type LicenseDoc = "license" | "notice" | "thirdPartyNotices";

/** 待套用遊戲更新的摘要（Rust `PendingSummary`）。 */
export interface PendingSummary {
  patchCount: number;
  totalBytes: number;
}

/** `check_game_update` 的結果；loginRequired＝國際服清單要登入才拿得到，目前沒有 pending。 */
export type GameUpdateStatus =
  | { kind: "upToDate" }
  | ({ kind: "updateAvailable" } & PendingSummary)
  | { kind: "loginRequired" };

/** `update_game` 的進度（Rust `GameUpdateProgress`）；patchIndex 從 0 起算，done／total 為該 patch 的 bytes。 */
export type GameUpdateProgress =
  | { kind: "download"; patchIndex: number; patchCount: number; done: number; total: number }
  | { kind: "verify"; patchIndex: number; patchCount: number }
  | { kind: "elevate" }
  | { kind: "apply"; patchIndex: number; patchCount: number; done: number; total: number };

/** 單一區服的更新狀態：dirty＝上次更新在套用途中中斷（拒絕啟動與更新，待使用者確認清除）。 */
export interface RegionPatchState {
  dirty: boolean;
  pending: PendingSummary | null;
}

export type PatchState = Record<Region, RegionPatchState>;

/**
 * 國際服完整性檢查的結果（Rust `IntegrityOutcome`）。checked：missing／mismatched 為對照原樣的 key
 * （如 `\game\ffxiv_dx11.exe`），checked 為對照列出的項目數；noReference：該版本沒有對照資料（404）。
 */
export type IntegrityOutcome =
  | { kind: "checked"; gameVersion: string; missing: string[]; mismatched: string[]; checked: number }
  | { kind: "noReference"; gameVersion: string };

export const regionLabel = (r: Region): string => (r === "global" ? "國際服" : "繁中服");

/** 各區服的 Dalamud 來源，也是 `InstalledVersion.source`。 */
const REGION_SOURCE: Record<Region, DalamudChannel["source"]> = { global: "goatcorp", tw: "yanmucorp" };

export const channelRegion = (c: DalamudChannel): Region =>
  c.source === "goatcorp" ? "global" : "tw";

/** 台服可選版本；國際服固定最新版，為 null。 */
export const channelVersion = (c: DalamudChannel): YanmucorpVersion | null =>
  c.source === "yanmucorp" ? c.version : null;

/** 區服對應的通道；version 只有台服用得到。 */
export const regionChannel = (r: Region, version: YanmucorpVersion): DalamudChannel =>
  r === "global" ? { source: "goatcorp" } : { source: "yanmucorp", version };

export const isRegionInstall = (v: InstalledVersion, r: Region): boolean => v.source === REGION_SOURCE[r];

export const api = {
  getConfig: () => invoke<ConfigView>("get_config"),
  getServerStatus: () => invoke<ServerStatus>("get_server_status"),
  openFolder: (target: FolderTarget) => invoke<void>("open_folder", { target }),
  /** 繁中服官網最新 5 則消息（新到舊）；取不到時 reject，沒有消息時為空陣列。 */
  getTwNews: () => invoke<NewsItem[]>("get_tw_news"),
  /** 國際服官方公告最新 5 則（新到舊），語言跟帳號的客戶端語言；取不到時 reject，沒有公告時為空陣列。 */
  getGlobalNews: (language: GlobalLanguage) => invoke<NewsItem[]>("get_global_news", { language }),
  /** 以系統瀏覽器開啟 region 的消息；後端只接受該服官方網站白名單內的 https 網址，其他一律拒絕。 */
  openNews: (region: Region, url: string) => invoke<void>("open_news", { region, url }),
  /** 以系統瀏覽器開啟 region 的官方商城（國際服 Mog Station、繁中服水晶商城）；網址由後端決定。 */
  openShop: (region: Region) => invoke<void>("open_shop", { region }),
  /** 以系統瀏覽器開啟 README 的國際服外部包示範影片段；網址由後端決定。 */
  openPackDemo: () => invoke<void>("open_pack_demo"),
  setGamePath: (region: Region, path: string | null) =>
    invoke<void>("set_game_path", { region, path }),
  /** 儲存主題並套到視窗標題列與 WebView2 的 prefers-color-scheme；頁面的 data-theme 由前端自己切。 */
  setTheme: (theme: Theme) => invoke<void>("set_theme", { theme }),
  /** 儲存「遊戲啟動後」；下次本程式成功啟動遊戲時生效。 */
  setAfterLaunch: (afterLaunch: AfterLaunch) => invoke<void>("set_after_launch", { afterLaunch }),
  /**
   * 儲存國際服外部包。只在 enabled 時檢查資料夾（不合格回給使用者看的原因）；還沒確認過風險時，開啟要帶
   * acceptRisk=true，否則後端拒絕。確認只記一次。
   */
  setExternalPack: (dir: string | null, enabled: boolean, acceptRisk: boolean) =>
    invoke<void>("set_external_pack", { dir, enabled, acceptRisk }),
  detectGamePath: (region: Region) => invoke<string | null>("detect_game_path", { region }),
  /**
   * 開系統的選擇資料夾對話框（主視窗為擁有者，title 是對話框標題，start 存在時從那裡開始）；回傳選到的路徑，取消為 null。
   * 只回傳路徑、不儲存。
   */
  pickFolder: (title: string, start: string | null) => invoke<string | null>("pick_folder", { title, start }),
  /** 只改帳號套用的 Dalamud 設定檔；回傳綁定後的設定檔 id，none 為 null。 */
  setAccountProfile: (accountId: string, target: BindTarget) =>
    invoke<string | null>("set_account_profile", { accountId, target }),
  saveAccount: (input: AccountInput) => invoke<string>("save_account", { input }),
  deleteAccount: (id: string) => invoke<void>("delete_account", { id }),
  saveProfile: (profile: Profile) => invoke<string>("save_profile", { profile }),
  deleteProfile: (id: string) => invoke<void>("delete_profile", { id }),
  /**
   * 複製設定檔：設定與插件本體（pluginConfigs 只取最上層檔案；插件資料與快取、log 不複製）一律複製進本程式的
   * profiles\<新 id>\，設定沿用、帳號綁定不複製。
   * name 為 null 時用「<原名> 複本」，重名自動加編號。可用 cancelDalamudDownload 取消。
   */
  duplicateProfile: (id: string, name: string | null, onProgress: (p: Progress) => void) => {
    const channel = new Channel<Progress>();
    channel.onmessage = onProgress;
    return invoke<DuplicatedProfile>("duplicate_profile", { id, name, onProgress: channel });
  },
  detectDalamudData: () => invoke<DetectedDalamudData[]>("detect_dalamud_data"),
  /** 沿用偵測到的舊資料：找到或新增對應的設定檔（name 非 null 時一併改名），回傳設定檔 id。 */
  importDetectedProfile: (path: string, name: string | null) =>
    invoke<string>("import_detected_profile", { path, name }),
  listYanmucorpReleases: () => invoke<ReleaseInfo[]>("list_yanmucorp_releases"),
  listInstalledDalamud: () => invoke<InstalledVersion[]>("list_installed_dalamud"),
  removeInstalledDalamud: (source: string, version: string) =>
    invoke<void>("remove_installed_dalamud", { source, version }),
  /** global 忽略 version（一律最新版）；tw 必填。回傳安裝好的版本。 */
  installDalamud: (region: Region, version: YanmucorpVersion | null, versionCheck: GameVersionCheck) =>
    invoke<string>("install_dalamud", { region, version, versionCheck }),
  /** 取消進行中的下載（Dalamud、程式更新、遊戲更新的下載／核對）、完整性檢查或設定檔複製；被取消的流程以 CANCELLED_MESSAGE 失敗。沒有進行中的工作時為 no-op。 */
  cancelDalamudDownload: () => invoke<void>("cancel_dalamud_download"),
  /** 檢查程式更新要帶的 updater target：免安裝版 `windows-x86_64-portable`，安裝版 null（plugin 預設）。 */
  appUpdateTarget: () => invoke<string | null>("app_update_target"),
  /**
   * 後端重新查詢最新版，分段下載、驗簽後安裝。安裝版：成功時本程式直接結束，不會 resolve。免安裝版：新版開始執行後
   * resolve，本程式隨即結束。
   */
  installAppUpdate: (onProgress: (p: UpdateProgress) => void) => {
    const channel = new Channel<UpdateProgress>();
    channel.onmessage = onProgress;
    return invoke<void>("install_app_update", { onProgress: channel });
  },
  /** 內嵌在程式裡的授權文件全文（THIRD-PARTY-NOTICES 約 570 KB，打開時才取）。 */
  getLicenseText: (doc: LicenseDoc) => invoke<string>("get_license_text", { doc }),
  /**
   * password 非空＝本次使用（不儲存），否則用已存的密碼。otp 只用於國際服手動 OTP；繁中服在後端開的登入確認視窗
   * 按「登入」（手動 OTP 也在那裡輸入），使用者按「取消」或關窗時以 CANCELLED_MESSAGE 失敗。
   */
  launchAccount: (
    accountId: string,
    otp: string | null,
    password: string | null,
    withoutDalamud: boolean,
    plugins: PluginLoad,
    versionCheck: GameVersionCheck,
  ) =>
    invoke<LaunchOutcome>("launch_account", {
      accountId,
      options: { otp, password, withoutDalamud, plugins, versionCheck },
    }),
  /** 預覽目前的 OTP：secret 非空白就用它（不儲存），否則用帳號已存的 OTP 金鑰。 */
  previewOtp: (accountId: string | null, secret: string | null) =>
    invoke<OtpPreview>("preview_otp", { accountId, secret }),
  /** 台服送一次版本檢查並更新 pending；國際服只回目前的 pending（清單要登入才拿得到）。 */
  checkGameUpdate: (region: Region) => invoke<GameUpdateStatus>("check_game_update", { region }),
  /** 下載、核對並套用該區服 pending 的遊戲更新；國際服完成後要再登入一次。 */
  updateGame: (region: Region, onProgress: (p: GameUpdateProgress) => void) => {
    const channel = new Channel<GameUpdateProgress>();
    channel.onmessage = onProgress;
    return invoke<void>("update_game", { region, onProgress: channel });
  },
  /** 使用者確認已用官方啟動器修復後，清除該區服的更新中斷標記。 */
  clearPatchDirty: (region: Region) => invoke<void>("clear_patch_dirty", { region }),
  getPatchState: () => invoke<PatchState>("get_patch_state"),
  /** 國際服完整性檢查（唯讀，不修復）。 */
  checkGameIntegrity: (onProgress: (p: Progress) => void) => {
    const channel = new Channel<Progress>();
    channel.onmessage = onProgress;
    return invoke<IntegrityOutcome>("check_game_integrity", { onProgress: channel });
  },
  getActSettings: () => invoke<ActConfig>("get_act_settings"),
  /** 只改 region 那一組；path 非 null 時須為存在的 .exe 完整路徑；launchWithGame 為 true 時須有 path。 */
  saveActSettings: (region: Region, path: string | null, launchWithGame: boolean) =>
    invoke<void>("save_act_settings", { region, path, launchWithGame }),
  /** 兩服 ACT 下拉選單的選項（系統安裝的 ACT、雙開 ACT、設定裡的其他路徑，依此順序）。讀登錄與檔案。 */
  listActChoices: () => invoke<ActChoice[]>("list_act_choices"),
  /** 各區服 ACT 狀態與仍在執行的遊戲；會列舉系統程序（ACT 判定），前端定時輪詢。 */
  getRunningStatus: () => invoke<RunningStatus>("get_running_status"),
  /** 以 ShellExecute 開啟 region 那組 ACT（ACT 要求系統管理員時由 Windows 跳 UAC）；已在執行就不再開。 */
  launchAct: (region: Region) => invoke<ActLaunch>("launch_act", { region }),
  /** ACT 連動狀態：各 ACT 是否已登記外掛、本程式建立的雙開 ACT。只讀檔案。 */
  getActLinkStatus: () => invoke<ActLinkStatus>("get_act_link_status"),
  /** 各區服 ACT 的 FFXIV 插件版本與最新版；最新版後端快取一小時，force＝重查。只讀檔案。 */
  getActPluginVersions: (force: boolean) => invoke<ActPluginVersions>("get_act_plugin_versions", { force }),
  /** 把連動外掛登記到每個已設定 ACT 的設定檔；有 ACT 在執行就失敗（請先關閉所有 ACT）。 */
  enableActLink: () => invoke<ActChange[]>("enable_act_link"),
  /** 以主 ACT 為底建立另一服用的 ACT，並寫回兩服的 ACT 設定；有 ACT 在執行就失敗。 */
  createDualAct: () => invoke<ActDualResult>("create_dual_act"),
  /** 兩服的角色、自訂名稱、使用標記與備份。只讀檔案。 */
  listCharacters: () => invoke<CharacterList>("list_characters"),
  /** 設定角色的自訂名稱；空白＝刪除。 */
  setCharacterLabel: (region: Region, id: string, name: string) =>
    invoke<void>("set_character_label", { region, id, name }),
  /** 手動備份角色（只讀遊戲檔，遊戲執行中也可以）；回備份 id。 */
  backupCharacter: (region: Region, id: string) => invoke<string>("backup_character", { region, id }),
  /** 把角色還原成 backupId；該區服遊戲或官方啟動器執行中、或沒設定遊戲路徑時失敗。回寫入前的自動備份 id。 */
  restoreCharacter: (region: Region, id: string, backupId: string) =>
    invoke<string>("restore_character", { region, id, backupId }),
  /** 轉移計畫（只讀）；來源與目標相同時失敗。 */
  planCharacterTransfer: (from: CharacterKey, to: CharacterKey) =>
    invoke<CategoryPlan[]>("plan_character_transfer", { from, to }),
  /** 轉移 categories 中可轉的分類（寫入前自動備份目標）；目標區服的遊戲執行中時失敗。 */
  transferCharacter: (from: CharacterKey, to: CharacterKey, categories: TransferCategory[]) =>
    invoke<TransferResult>("transfer_character", { from, to, categories }),
};
