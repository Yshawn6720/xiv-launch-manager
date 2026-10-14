//! 台服：登入確認對話框（reCAPTCHA token）、登入、版本檢查、啟動參數、World 狀態、官方新聞（protocols §1.4.1、§2）。

pub mod login;
// generate_handler! 依函式路徑找同模組的 command 巨集，故 `tw_recaptcha_event` 不在此 re-export，註冊時寫 `tw::recaptcha::tw_recaptcha_event`。
pub mod recaptcha;
mod news;
mod world_status;

pub use login::{launch_args, login};
pub use news::{NEWS_HOSTS, fetch_news};
pub use recaptcha::{LoginDialog, RecaptchaState, acquire_token};
pub use world_status::fetch_world_status;
