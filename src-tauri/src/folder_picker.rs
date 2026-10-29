//! 系統的「選擇資料夾」對話框（Windows `IFileOpenDialog` 的 `FOS_PICKFOLDERS`），給設定頁的路徑輸入框用：只回傳
//! 使用者選的路徑，由前端填入輸入框；儲存與檢查照舊走各自的 command。

use std::path::Path;

use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, IShellItem,
    SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
};
use windows::core::{HRESULT, HSTRING};

use crate::error::{AppError, AppResult};

/// 以 `owner` 為擁有者開啟選擇資料夾對話框（期間 `owner` 不能操作），`title` 是對話框標題。`start` 是存在的資料夾時從那裡
/// 開始，否則用系統預設的位置。回傳選到的完整路徑；使用者取消回 `None`。
///
/// 必須在已初始化 COM（STA）且有訊息迴圈的執行緒呼叫：同步 command 在主執行緒（WebView2 所在的 STA 執行緒）執行。
/// 對話框是 modal，`Show` 自己跑訊息迴圈，關閉前不返回。
pub fn pick_folder(owner: HWND, title: &str, start: Option<&Path>) -> AppResult<Option<String>> {
    let fail = |e: windows::core::Error| AppError::Io {
        context: "開啟選擇資料夾對話框".into(),
        message: e.message(),
    };
    // SAFETY: 下列都是 COM 方法呼叫，介面指標由 windows crate 管理參照計數；字串參數是本函式持有、以 0 結尾、呼叫期間存活的
    // HSTRING。呼叫端保證在 STA 執行緒（見函式說明）。quality-allow: Win32 COM FFI，IFileOpenDialog 沒有安全包裝
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).map_err(fail)?;
        let options = dialog.GetOptions().map_err(fail)?;
        dialog
            .SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST)
            .map_err(fail)?;
        dialog.SetTitle(&HSTRING::from(title)).map_err(fail)?;
        // 起始資料夾只是方便：路徑不存在或解析不了時用系統預設位置，不算失敗。
        if let Some(dir) = start.filter(|d| d.is_dir())
            && let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(dir.as_os_str()), None)
        {
            dialog.SetFolder(&item).map_err(fail)?;
        }
        match dialog.Show(Some(owner)) {
            Ok(()) => {}
            Err(e) if e.code() == HRESULT::from_win32(ERROR_CANCELLED.0) => return Ok(None),
            Err(e) => return Err(fail(e)),
        }
        let item = dialog.GetResult().map_err(fail)?;
        let raw = item.GetDisplayName(SIGDN_FILESYSPATH).map_err(fail)?;
        let path = raw.to_string();
        CoTaskMemFree(Some(raw.0.cast_const().cast()));
        path.map(Some).map_err(|e| AppError::Io {
            context: "讀取選擇的資料夾路徑".into(),
            message: e.to_string(),
        })
    }
}
