//! 讀取 PE 檔版本資源的 FileVersion（`VS_FIXEDFILEINFO`）。

use std::ffi::c_void;
use std::path::Path;

use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW,
};
use windows::core::{HSTRING, w};

/// `VS_FIXEDFILEINFO::dwSignature` 的固定值。
const FIXED_FILE_INFO_SIGNATURE: u32 = 0xFEEF_04BD;

/// 回傳 `a.b.c.d` 形式的 FileVersion；檔案不存在、不是 PE 檔或沒有版本資源時回 `None`。
pub fn file_version(path: &Path) -> Option<String> {
    let path = HSTRING::from(path);
    // SAFETY: path 是以 0 結尾、在呼叫期間存活的 UTF-16 字串；lpdwhandle 可為 None。
    // quality-allow: Win32 FFI，版本資源 API 沒有安全包裝
    let size = unsafe { GetFileVersionInfoSizeW(&path, None) };
    if size == 0 {
        return None;
    }
    let mut block = vec![0u8; usize::try_from(size).ok()?];
    // SAFETY: block 是本函式持有、長度恰為 size 位元組的可寫緩衝區。
    // quality-allow: Win32 FFI，版本資源 API 沒有安全包裝
    unsafe { GetFileVersionInfoW(&path, None, size, block.as_mut_ptr().cast()) }.ok()?;

    let mut info: *mut c_void = std::ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: block 是 GetFileVersionInfoW 填好的版本資源；成功時 info 指向 block 內部，
    // 長度 len 位元組。quality-allow: Win32 FFI，版本資源 API 沒有安全包裝
    let found = unsafe { VerQueryValueW(block.as_ptr().cast(), w!("\\"), &mut info, &mut len) };
    if !found.as_bool() || info.is_null() || (len as usize) < size_of::<VS_FIXEDFILEINFO>() {
        return None;
    }
    // SAFETY: 上面確認 info 非空且指向 block 內至少 size_of::<VS_FIXEDFILEINFO>() 位元組；
    // block 仍存活。Vec<u8> 不保證 4 位元組對齊，故用 read_unaligned。
    // quality-allow: 讀取 FFI 回傳、位於本地緩衝區內的結構
    let fixed = unsafe { info.cast::<VS_FIXEDFILEINFO>().read_unaligned() };
    if fixed.dwSignature != FIXED_FILE_INFO_SIGNATURE {
        return None;
    }
    Some(format!(
        "{}.{}.{}.{}",
        fixed.dwFileVersionMS >> 16,
        fixed.dwFileVersionMS & 0xFFFF,
        fixed.dwFileVersionLS >> 16,
        fixed.dwFileVersionLS & 0xFFFF,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_system_dll_version() {
        let root = std::env::var_os("SystemRoot").expect("SystemRoot 環境變數");
        let kernel32 = Path::new(&root).join("System32").join("kernel32.dll");
        let version = file_version(&kernel32).expect("kernel32.dll 應有版本資源");
        let parts: Vec<&str> = version.split('.').collect();
        assert_eq!(parts.len(), 4, "{version}");
        assert!(parts.iter().all(|p| p.parse::<u16>().is_ok()), "{version}");
    }

    #[test]
    fn missing_or_non_pe_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(file_version(&dir.path().join("missing.dll")), None);
        let text = dir.path().join("Dalamud.dll");
        std::fs::write(&text, "not a PE file").unwrap();
        assert_eq!(file_version(&text), None);
    }
}
