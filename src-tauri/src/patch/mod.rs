//! 遊戲更新（protocols §5）：patch list 解析、區塊 SHA1 核對、ZiPatch 套用、提權 helper 與完整性檢查。

pub mod access;
pub mod apply;
pub mod dirty;
pub mod helper;
pub mod integrity;
pub mod job;
pub mod list;
pub mod running;
pub mod update;
pub mod zipatch;
