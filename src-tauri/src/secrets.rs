//! 密碼與 OTP 金鑰存放在 Windows Credential Manager（generic credential）。
//!
//! 只防設定檔外洩；同一 Windows 使用者下的程序仍可讀取（主 note「憑證與 secret」）。
//! 錯誤訊息不帶 secret 內容。

use windows::Win32::Foundation::ERROR_NOT_FOUND;
use windows::Win32::Security::Credentials::{
    CRED_FLAGS, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree,
    CredReadW, CredWriteW,
};
use windows::core::{HRESULT, PCWSTR, PWSTR};

use crate::error::{AppError, AppResult};

const SERVICE: &str = "xiv-launch-manager";

#[derive(Debug, Clone, Copy)]
pub enum SecretKind {
    Password,
    OtpSecret,
}

impl SecretKind {
    fn suffix(self) -> &'static str {
        match self {
            SecretKind::Password => "password",
            SecretKind::OtpSecret => "otp",
        }
    }
}

fn target(account_id: &str, kind: SecretKind) -> Vec<u16> {
    wide(&format!("{SERVICE}/{account_id}/{}", kind.suffix()))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn is_not_found(err: &windows::core::Error) -> bool {
    err.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0)
}

fn secret_error(context: &str) -> AppError {
    AppError::Secret {
        context: context.into(),
    }
}

pub fn store(account_id: &str, kind: SecretKind, value: &str) -> AppResult<()> {
    let mut target = target(account_id, kind);
    let mut user = wide(account_id);
    // blob 存 UTF-8；不含結尾 0。
    let mut blob = value.as_bytes().to_vec();
    let blob_len = u32::try_from(blob.len()).map_err(|_| secret_error("憑證內容過長"))?;
    let credential = CREDENTIALW {
        Flags: CRED_FLAGS(0),
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_mut_ptr()),
        CredentialBlobSize: blob_len,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: PWSTR(user.as_mut_ptr()),
        ..Default::default()
    };
    // SAFETY: credential 內的指標指向本函式持有、呼叫期間存活的 target／user／blob 緩衝區；
    // CredWriteW 只在呼叫期間讀取它們。quality-allow: Credential Manager 只有 Win32 FFI 可用
    unsafe { CredWriteW(&credential, 0) }.map_err(|_| secret_error("寫入憑證"))
}

/// 讀取；不存在回傳 `None`，其他失敗回報錯誤。
pub fn load(account_id: &str, kind: SecretKind) -> AppResult<Option<String>> {
    let target = target(account_id, kind);
    let mut out: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: target 是以 0 結尾、在呼叫期間存活的 UTF-16 字串；成功時 out 指向系統配置的
    // CREDENTIALW，下方讀完後以 CredFree 釋放。quality-allow: Credential Manager 只有 Win32 FFI 可用
    let read = unsafe { CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None, &mut out) };
    match read {
        Ok(()) => {
            // SAFETY: CredReadW 成功保證 out 非空，且 CredentialBlob 指向
            // CredentialBlobSize 個有效位元組，直到 CredFree 為止。quality-allow: 讀取 FFI 回傳的緩衝區
            let value = unsafe {
                let cred = &*out;
                let bytes = if cred.CredentialBlob.is_null() {
                    &[][..]
                } else {
                    std::slice::from_raw_parts(cred.CredentialBlob, cred.CredentialBlobSize as usize)
                };
                let decoded = String::from_utf8(bytes.to_vec());
                CredFree(out as *const core::ffi::c_void);
                decoded
            };
            value.map(Some).map_err(|_| secret_error("憑證內容不是 UTF-8"))
        }
        Err(e) if is_not_found(&e) => Ok(None),
        Err(_) => Err(secret_error("讀取憑證")),
    }
}

/// 刪除；本來就不存在視為成功。
pub fn delete(account_id: &str, kind: SecretKind) -> AppResult<()> {
    let target = target(account_id, kind);
    // SAFETY: target 是以 0 結尾、在呼叫期間存活的 UTF-16 字串。quality-allow: Credential Manager 只有 Win32 FFI 可用
    match unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None) } {
        Ok(()) => Ok(()),
        Err(e) if is_not_found(&e) => Ok(()),
        Err(_) => Err(secret_error("刪除憑證")),
    }
}

pub fn exists(account_id: &str, kind: SecretKind) -> AppResult<bool> {
    load(account_id, kind).map(|v| v.is_some())
}
