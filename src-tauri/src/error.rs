//! 全專案共用的錯誤型別，會序列化後回傳前端。
//!
//! 不變量：任何 variant 的欄位都不得含密碼、OTP、SID／UID、reCAPTCHA token、
//! 遊戲參數或登入回應原文。網路錯誤一律經 `AppError::network`
//! 去掉 URL，因為國際服 session 註冊的 URL path 含 SID。

use serde::{Serialize, Serializer};

use crate::region::Region;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("網路錯誤（{context}）：{message}")]
    Network { context: String, message: String },
    #[error("伺服器回應格式不符預期（{context}）")]
    Protocol { context: String },

    #[error("{region} 登入被拒：{message}")]
    LoginRejected { region: Region, message: String },
    #[error("帳號尚未同意服務條款，請先用官方啟動器登入一次")]
    TermsNotAccepted,
    #[error("帳號目前無法遊玩（可能未訂閱）")]
    NotPlayable,
    #[error("繁中服 reCAPTCHA 失敗：{context}")]
    Recaptcha { context: String },
    /// 使用者在登入確認視窗按「取消」或關窗。前端以訊息等於「已取消」辨識（`src/api.ts` `CANCELLED_MESSAGE`），
    /// 不當成登入失敗；改訊息須同步改前端。
    #[error("已取消")]
    RecaptchaCancelled,

    #[error("{region} 遊戲需要更新，請先用 {region} 官方啟動器更新")]
    NeedsUpdate { region: Region },
    #[error("國際服 boot 檔與官方不符，請用國際服官方啟動器修復")]
    BootModified,
    #[error("{region} 版本檢查失敗（HTTP {status}）")]
    VersionCheckFailed { region: Region, status: u16 },
    #[error("{region} 伺服器維修中，請稍後再試")]
    ServerMaintenance { region: Region },
    /// 原因只含規則與行號，不回顯清單內容或 URL。
    #[error("{region} patch 清單不符規格，整批拒絕（{reason}）")]
    PatchListRejected { region: Region, reason: String },
    #[error("patch 檔核對失敗（{reason}）")]
    PatchVerifyFailed { reason: String },
    /// ZiPatch 內容或輸出路徑違反 protocols §5.4；原因只含規則與偏移，不回顯 patch 內容或路徑。
    #[error("patch 檔不符規格或輸出路徑不安全（{reason}）")]
    PatchRejected { reason: String },
    /// 版本檢查取得了 patch list（已存成 pending）：不啟動。前端以訊息結尾「，請先執行遊戲更新」辨識
    /// （`src/api.ts` `UPDATE_REQUIRED_SUFFIX`），接著自動開始遊戲更新；改訊息須同步改前端。
    #[error("{region} 遊戲需要更新（{patch_count} 個 patch，共 {} MB），請先執行遊戲更新", mib(.total_bytes))]
    UpdateRequired {
        region: Region,
        patch_count: usize,
        total_bytes: u64,
    },
    /// 更新判斷需要的 `.ver` 缺失：不以基礎版本兜底要清單（主 note「版本來源」）。
    #[error("找不到 {region} 版本檔 {path}，無法判斷是否需要更新；請先用 {region} 官方啟動器更新或修復")]
    VersionFileMissing { region: Region, path: String },
    /// 該安裝有套用途中中斷的遊戲更新（dirty 標記）；使用者修復後在本程式確認清除。
    #[error("{region} 遊戲更新曾在套用途中中斷，遊戲檔可能不完整；請先用 {region} 官方啟動器修復，修復後在本程式確認清除更新標記")]
    PatchDirty { region: Region },
    /// 目標安裝的遊戲或官方啟動器正在執行（protocols §3 例外的映像路徑比對）。
    #[error("遊戲或官方啟動器正在執行，請先關閉再更新")]
    GameRunning,
    #[error("{path} 所在磁碟剩餘空間不足（需要 {} MB，剩 {} MB）", mib(.required), mib(.available))]
    InsufficientDiskSpace {
        path: String,
        required: u64,
        available: u64,
    },
    /// 套用在寫入前中止（含 UAC 被拒）：已完成的 patch 保留，失敗與未開始的不推進，不留 dirty 標記。
    #[error("遊戲更新已中止（{reason}），請重新檢查更新後再試")]
    UpdateAborted { reason: String },
    /// 套用可能已寫入遊戲目錄後失敗：保留 dirty 標記，提示官方修復。
    #[error("{region} 遊戲更新失敗（{reason}），遊戲檔可能不完整；請用 {region} 官方啟動器修復，修復後在本程式確認清除更新標記")]
    UpdateBroken { region: Region, reason: String },

    #[error("尚未設定 {region} 遊戲路徑")]
    GameNotConfigured { region: Region },
    #[error("遊戲檔案不存在：{path}")]
    GameFileMissing { path: String },
    #[error("額外啟動參數第 {index} 項不是 key=value 格式")]
    InvalidExtraArgs { index: usize },

    #[error("ACT 執行檔路徑無效：{reason}")]
    InvalidActPath { reason: String },
    #[error("尚未設定 ACT 執行檔路徑")]
    ActNotConfigured,
    /// `ShellExecuteExW` 失敗；UAC 被拒也在這裡（reason 註明）。
    #[error("無法開啟 ACT：{reason}")]
    ActLaunchFailed { reason: String },
    /// 「啟用 ACT 連動」「建立另一服的 ACT」執行時有 ACT 在跑：ACT 結束時會以記憶體中的設定覆寫設定檔。
    #[error("請先關閉所有 ACT 再設定（ACT 關閉時會覆寫自己的設定檔）")]
    ActRunning,
    /// ACT 連動或雙開設定做不下去（設定檔不存在、FFXIV 插件未設定等）；reason 是給使用者的說明。
    #[error("{reason}")]
    ActSetup { reason: String },

    #[error("國際服 Dalamud 已被官方暫停使用；可在登入頁「進階 › 本次載入方式」選「不載入 Dalamud」後再登入")]
    DalamudRemoteDisabled,
    #[error("無法確認國際服 Dalamud 是否可用，已停止登入；可稍後再試，或在登入頁「進階 › 本次載入方式」選「不載入 Dalamud」後再登入")]
    DalamudLeaseUnavailable,
    #[error("Dalamud 目前只支援遊戲版本 {supported}，本機遊戲已是 {installed}（遊戲剛改版，需等 Dalamud 更新）；可在登入頁「進階 › 本次載入方式」選「不載入 Dalamud」後再登入，或選「強制載入（忽略遊戲版本）」自行承擔遊戲崩潰的風險")]
    DalamudGameVersionMismatch { supported: String, installed: String },
    #[error("檔案完整性核對失敗：{file}")]
    Integrity { file: String },
    #[error("安裝失敗（{context}）")]
    Install { context: String },
    #[error("Injector 失敗（exit code {exit_code:?}）")]
    InjectorFailed { exit_code: Option<i32> },
    #[error("遊戲啟動後立即結束（可能超過客戶端實例上限）")]
    GameExitedEarly,

    /// 登入時設定檔正被本程式啟動的另一個遊戲使用、且還沒確認過共用（protocols §3）。前端登入前會先問；只有
    /// 前端檢查之後才開始被使用時會走到這裡，再按一次登入就會問。
    #[error("Dalamud 設定檔「{profile}」正被另一個遊戲使用，尚未確認共用；請再按一次「登入」")]
    ProfileInUse { profile: String },
    /// 沿用的舊啟動器資料夾已被另一個設定檔沿用。
    #[error("這個 Dalamud 資料夾已被設定檔「{profile}」沿用")]
    DataDirTaken { profile: String },
    #[error("仍有帳號使用這個 Dalamud 設定檔，請先讓這些帳號改用其他設定檔")]
    ProfileHasAccounts,
    /// 複製時來源正被遊戲使用：衛月會持續寫入設定，複製到的可能是寫到一半的檔案。
    #[error("Dalamud 設定檔「{profile}」正被執行中的遊戲使用，請先關閉遊戲再複製")]
    DuplicateProfileRunning { profile: String },
    #[error("Dalamud 設定檔與帳號的區服不同")]
    ProfileRegionMismatch,
    #[error("找不到 {what}")]
    NotFound { what: String },
    /// 本次沒有輸入密碼，且帳號未儲存密碼（每次手動輸入模式）。
    #[error("請輸入密碼")]
    PasswordRequired,
    /// `open_news` 收到不在該服白名單內的網址（protocols §1.4.2、§2.7）；不回顯網址。
    #[error("只能開啟{region}官方網站的消息連結")]
    NewsLinkRejected { region: Region },

    /// 角色資料寫入互斥（protocols §7.3）：同區服有遊戲或官方啟動器在執行。
    #[error("{region} 的遊戲或官方啟動器正在執行，請先關閉再寫入角色資料")]
    CharacterDataInUse { region: Region },
    /// 前端傳來的角色 id 或備份 id 不符格式（不回顯原文）。
    #[error("角色或備份的識別碼格式不符")]
    InvalidCharacterId,
    #[error("轉移的來源與目標是同一個角色")]
    TransferSameCharacter,
    /// 角色資料夾或要寫的檔是 symlink／junction：備份不收連結，寫入會把連結換成一般檔，所以不寫。
    #[error("{path} 是連結（symlink 或 junction），不寫入角色資料；請改成一般檔案或資料夾後再試")]
    CharacterLinked { path: String },
    /// 角色資料寫到一半失敗，已以寫入前的自動備份恢復成原狀。
    #[error("寫入角色資料失敗，已恢復成寫入前（{reason}）")]
    CharacterWriteRolledBack { reason: String },
    /// 寫到一半失敗、恢復也失敗：寫入前的自動備份改成手動備份（不會被保留規則刪掉）；`kept` 說明改名是否成功。
    /// `created` 是這次新建、可能留在角色資料夾的檔名（空字串＝沒有）。
    #[error("寫入角色資料失敗，也無法自動恢復（{reason}）；寫入前的備份 {backup} {kept}，請用「還原」恢復{created}")]
    CharacterRollbackFailed { reason: String, backup: String, kept: String, created: String },

    #[error("檔案操作失敗（{context}）：{message}")]
    Io { context: String, message: String },
    #[error("憑證存取失敗（{context}）")]
    Secret { context: String },
    /// 使用者取消下載（Dalamud 或遊戲更新）；前端以此訊息原文辨識，改字要同步前端。
    #[error("已取消")]
    Cancelled,

    /// 程式更新的檢查、讀設定或啟動安裝程式失敗（updater plugin 的錯誤訊息不含 secret）。
    #[error("{context}：{message}")]
    Updater { context: String, message: String },
    /// 下載的更新檔沒通過簽章驗證，不安裝。
    #[error("更新檔簽章驗證失敗（{reason}）")]
    UpdateSignature { reason: String },
    /// 免安裝版沒辦法自己換成新版（資料夾不能寫、改名或啟動被擋等）；已還原，目前的 exe 照常可用。
    #[error("免安裝版無法自動更新（{reason}）；目前的版本仍可使用，請到 GitHub Releases（github.com/Yshawn6720/xiv-launch-manager/releases）手動下載新版")]
    PortableUpdate { reason: String },
    /// 替換到一半失敗、連還原也失敗：原檔名下沒有可用的 exe，舊版留在 `backup`。
    #[error("免安裝版更新失敗，也無法還原（{reason}）：請把 {backup} 改名回 {exe}，或到 GitHub Releases（github.com/Yshawn6720/xiv-launch-manager/releases）重新下載")]
    PortableUpdateStranded {
        reason: String,
        exe: String,
        backup: String,
    },
}

/// 前端只需要可顯示的訊息；Display 已保證不含 secret（見檔頭不變量）。
impl Serialize for AppError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl AppError {
    /// 網路錯誤：去掉 URL 再轉字串（URL 可能含 SID）。reqwest 的 Display 只有「error sending
    /// request」，根因在 source chain（hyper／io／rustls，最多帶主機名，不含 path），一併串上。
    pub fn network(context: impl Into<String>, err: reqwest::Error) -> Self {
        let err = err.without_url();
        let mut message = err.to_string();
        let mut source = std::error::Error::source(&err);
        while let Some(cause) = source {
            message.push('：');
            message.push_str(&cause.to_string());
            source = cause.source();
        }
        AppError::Network {
            context: context.into(),
            message,
        }
    }

    pub fn io(context: impl Into<String>, err: std::io::Error) -> Self {
        AppError::Io {
            context: context.into(),
            message: err.to_string(),
        }
    }

    pub fn protocol(context: impl Into<String>) -> Self {
        AppError::Protocol {
            context: context.into(),
        }
    }
}

/// 錯誤訊息用的 MB（向上取整）。
fn mib(bytes: &u64) -> u64 {
    bytes.div_ceil(1024 * 1024)
}

pub type AppResult<T> = Result<T, AppError>;
