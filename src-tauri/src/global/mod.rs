//! 國際服：Lease、閘道狀態、登入、session 註冊、啟動參數加密、官方公告（protocols §1）、外部包與影子資料夾（§1.6）。

mod args;
mod blowfish;
mod gate;
mod login;
mod news;
pub mod shadow;

pub use args::launch_args;
pub use gate::{ServerStatus, fetch_gate_status};
pub use login::{Lease, check_boot, fetch_lease, login};
pub use news::{NEWS_HOSTS, fetch_news};
