//! RFC 6238 TOTP（HMAC-SHA1、6 位、30 秒），兩服的 Square Enix／智冠 OTP 皆同規格。

use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

use crate::error::{AppError, AppResult};

const STEP_SECONDS: u64 = 30;
const DIGITS: u32 = 6;

/// 解析使用者貼上的 base32 金鑰：忽略空白與大小寫、允許省略 `=` 補位。
pub fn parse_secret(input: &str) -> AppResult<Vec<u8>> {
    let normalized: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '=')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    data_encoding::BASE32_NOPAD
        .decode(normalized.as_bytes())
        .ok()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| AppError::Secret {
            context: "OTP 金鑰不是有效的 base32".into(),
        })
}

pub fn code_at(key: &[u8], unix_seconds: u64) -> String {
    hotp(key, unix_seconds / STEP_SECONDS)
}

/// 指定時間的 OTP 與這組 OTP 還剩幾秒有效（1–30）。
pub fn code_with_seconds_left(key: &[u8], unix_seconds: u64) -> (String, u64) {
    (code_at(key, unix_seconds), STEP_SECONDS - unix_seconds % STEP_SECONDS)
}

pub fn current_code_with_seconds_left(key: &[u8]) -> (String, u64) {
    code_with_seconds_left(key, now())
}

/// 目前的 Unix 秒數（UTC）；產生 OTP 的時間點。
pub fn now() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

fn hotp(key: &[u8], counter: u64) -> String {
    // HMAC 接受任意長度金鑰，new_from_slice 不會失敗。
    let mut mac = <Hmac<Sha1> as KeyInit>::new_from_slice(key)
        .unwrap_or_else(|_| unreachable!("HMAC accepts keys of any length")); // quality-allow: HMAC 規格允許任意長度金鑰，此分支不可達
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[19] & 0x0f) as usize;
    let binary = u32::from_be_bytes([
        digest[offset] & 0x7f,
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]);
    format!("{:0width$}", binary % 10u32.pow(DIGITS), width = DIGITS as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6238 附錄 B，SHA1 金鑰 "12345678901234567890"，取 8 位結果的後 6 位。
    #[test]
    fn matches_rfc6238_vectors() {
        let key = b"12345678901234567890";
        for (t, expected8) in [
            (59u64, "94287082"),
            (1111111109, "07081804"),
            (1111111111, "14050471"),
            (1234567890, "89005924"),
            (2000000000, "69279037"),
            (20000000000, "65353130"),
        ] {
            assert_eq!(code_at(key, t), expected8[2..], "t={t}");
        }
    }

    #[test]
    fn seconds_left_counts_down_within_each_step() {
        let key = b"12345678901234567890";
        // RFC 6238 附錄 B：t=59 是第一組的最後一秒，t=60 進入下一組。
        assert_eq!(code_with_seconds_left(key, 59), ("287082".to_string(), 1));
        let (next, left) = code_with_seconds_left(key, 60);
        assert_eq!(left, 30);
        assert_eq!(next, code_at(key, 89));
        assert_ne!(next, "287082");
        assert_eq!(code_with_seconds_left(key, 0).1, 30);
        assert_eq!(code_with_seconds_left(key, 1111111109), ("081804".to_string(), 1));
        assert_eq!(code_with_seconds_left(key, 1111111111), ("050471".to_string(), 29));
    }

    #[test]
    fn secret_parsing_tolerates_spacing_case_and_padding() {
        // "12345678901234567890" 的 base32
        let canonical = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        let messy = "gezd gnbv gy3t qojq gezd gnbv gy3t qojq==";
        assert_eq!(parse_secret(canonical).expect("canonical"), b"12345678901234567890");
        assert_eq!(parse_secret(messy).expect("messy"), b"12345678901234567890");
        assert!(parse_secret("not base32!").is_err());
        assert!(parse_secret("   ").is_err());
    }
}
