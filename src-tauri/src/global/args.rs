//! 國際服啟動參數組裝與加密（protocols §1.2）。
//!
//! 產出的字串含 UID，屬遊戲參數：只能交給子程序命令列，不得寫進錯誤、log 或回傳前端。

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use super::blowfish;
use super::login::GlobalSession;
use crate::region::GlobalLanguage;

const CHECKSUM_TABLE: &[u8; 16] = b"fX1pGtdS5CAP4_VL";

/// 依 SE 格式加密參數：最前面插入 `T={ticks}`，key 為 `ticks & 0xFFFF0000` 的 8 位小寫 hex。
pub fn build_encrypted_args(ticks: u32, args: &[(&str, &str)]) -> String {
    let key = ticks & 0xFFFF_0000;
    let ticks_text = ticks.to_string();

    let mut plain = String::new();
    for (k, v) in std::iter::once(("T", ticks_text.as_str())).chain(args.iter().copied()) {
        // 客戶端以空白分隔參數，值內的空白以雙空白跳脫。
        plain.push_str(" /");
        plain.push_str(&k.replace(' ', "  "));
        plain.push_str(" =");
        plain.push_str(&v.replace(' ', "  "));
    }

    let cipher = blowfish::encrypt(format!("{key:08x}").as_bytes(), plain.as_bytes());
    let encoded: String = STANDARD
        .encode(cipher)
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            '=' => '*',
            other => other,
        })
        .collect();
    let checksum = char::from(CHECKSUM_TABLE[((key & 0x000F_0000) >> 16) as usize]);
    format!("//**sqex0003{encoded}{checksum}**//")
}

/// 國際服遊戲參數（已加密）；`extra` 接在標準參數之後一起加密（protocols §2.6）。
/// ticks 取開機毫秒數，與官方啟動器相同。
pub fn launch_args(
    session: &GlobalSession,
    game_version: &str,
    language: GlobalLanguage,
    extra: &[(String, String)],
) -> String {
    // SAFETY: GetTickCount 無參數、無前置條件，只讀取系統計數器。
    // quality-allow: Win32 FFI，windows crate 將所有 Win32 函式標為 unsafe
    let ticks = unsafe { windows::Win32::System::SystemInformation::GetTickCount() };
    let max_expansion = session.max_expansion.to_string();
    let region = session.region.to_string();
    let language = language.code().to_string();
    let mut args = vec![
        ("DEV.DataPathType", "1"),
        ("DEV.MaxEntitledExpansionID", max_expansion.as_str()),
        ("DEV.TestSID", session.unique_id.as_str()),
        ("DEV.UseSqPack", "1"),
        ("SYS.Region", region.as_str()),
        ("language", language.as_str()),
        ("resetConfig", "0"),
        ("ver", game_version),
    ];
    args.extend(extra.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    build_encrypted_args(ticks, &args)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// XL 7.0.20 `ArgumentBuilder.BuildEncrypted`＋`LegacyBlowfish` 產出的對照向量
    /// （C:/dev-tmp/scratch/xiv-oracle/vectors.tsv）。
    const VECTORS: &[(u32, &str, &str)] = &[
        (0, "launch", "//**sqex0003dDvxpwp6Q4Cu1Mf6pIYrV_IN8zx1PxJfur658Tc90lHJJo6rx_wuyU8AGCr0spP-VLNHUog9SIQTPN8YcJn9-Rru9B6cFiArnUsYf4O_2ySs6p2Dfhr4RZFFe4bYQRPerOqdg34a-EWRRXuG2EET3p3i-80ggR3ZBVwIRdjbRRrHPFIaPezT7WD8M-Q3-qkJgvUHKdgMFNmcnZNl6OYrNOIW9ZFdexdvO0DkovqwY9pd7kQLGstugtKSR4-GWac30_gBZZ6KqbmLPRUk0Cajdw**f**//"),
        (0, "spaced", "//**sqex0003dDvxpwp6Q4D309rzBCLHL8I_tFVbZj-5f**//"),
        (305419896, "launch", "//**sqex0003V4CSvwpH7dvAtpOVqv1QTN7nfW333ccgd1sXDx1QHKCf9aT8GPzepCWocXd_Cw0_yuB_b-KhMtcQSLR7xnDy7YoljfBaHgTFwbpdGhcOoWQs8NUyJTJVPUix-czY3h7y1odxg2V0ulxIsfnM2N4e8taHcYNldLpcUpO7uxmmOODsa3tZn0DqFS4s5KF7G0pm-VgMbCwLAyO6ANZyDMGA0a5fewaia10890HTWt1csky7ZjE1SQVSC0x6WxdV3LNo9ZpNKzBcBesR7HmDbus-Oh9FheO8T2hMG**//"),
        (305419896, "spaced", "//**sqex0003V4CSvwpH7dvAtpOVqv1QTHCuXYuhDf7ZGw3G2cjLVok*G**//"),
        (2415919103, "launch", "//**sqex0003iO1WL6YzRx5kc-J3NA-crFsYOb14sCxwp8SLY3vC52LyZTa3EEAU7F58UwAjd3ltgQAzuTY8xuIAIgKsAICVtrqyIhRqAkFXC9Sgk6n-1BBG0SoKphdH02f5j_mutY1pleOmBfquKXln-Y_5rrWNaZXjpgX6ril5L_wIf0RA1kUcFp2gUJFHS4OexTTmwWpWDvIg2RoyYMoU3ilwO5ActlcHkcAta_QLc__F5Xfjlz8YBAP4XplFE0Frc-smdqu0caghJw43I-BZyY8pcZ0VJK3OyFyBvLPvL**//"),
        (2415919103, "spaced", "//**sqex0003iO1WL6YzRx5kc-J3NA-crHi9HH_GC1syxxuthWcI1sE*L**//"),
        (4294967295, "launch", "//**sqex0003EwcYdmTyDI-PDFJZR64jzR78CeLw7puKGU4t3Ysk4cqZnXA1iC_dmr77ZfTACPhLYyTV8d68nqClzPE0KwuRkN0WCGbxssqQ_0BjXpC4daom7VjD96qhcBTizuh2BYa-zHApk76bx8wU4s7odgWGvsxwKZO-m8fMenryIxk7euNGcDoxWjZ332ULyYBjU-NDG-UjmpJfecQWp9LRd219aas7fRIDyNHSVYM0t-F-NeSwrtKw-2UrfVJHXcqfhCR_WA2sRpkOU9NXeNKKsU_wsOi-Gy_gXtTsL**//"),
        (4294967295, "spaced", "//**sqex0003EwcYdmTyDI-PDFJZR64jza-IS7M6qPEajsGYeYwN7kI*L**//"),
        (123456789, "launch", "//**sqex0003ofzFG_FsZu90xnXQhciAAiyrKpKnL9GodPSio0BoH7TIs0sWONxjvYuK04HF0Bf0WLSZk0b8sDFpgbmLHxA8g9HC42ZP7-FiMPVquAqPbiETE2GMoPIeCVeckxH3EPGEZzVG0-aU-zBXnJMR9xDxhGc1RtPmlPsw4LlE3OOCjrETMiVp5n6UT7lwHYIB8I8l55EbE7zZqbrJsY0kA6aQ6mMYTJkmEIyV3FCb998JBc_OLpZxRl-1aGYerNSyDYukpIeSFNPWfVkTdy6H_e4KHQPoa0_-23rQP**//"),
        (123456789, "spaced", "//**sqex0003ofzFG_FsZu90xnXQhciAAqeQi6JY2pr5wGxp_2TSNFQ*P**//"),
    ];

    const LAUNCH_ARGS: &[(&str, &str)] = &[
        ("DEV.DataPathType", "1"),
        ("DEV.MaxEntitledExpansionID", "5"),
        ("DEV.TestSID", "0123456789abcdef0123456789abcdef01234567"),
        ("DEV.UseSqPack", "1"),
        ("SYS.Region", "3"),
        ("language", "1"),
        ("resetConfig", "0"),
        ("ver", "2026.09.15.0000.0000"),
    ];

    const SPACED_ARGS: &[(&str, &str)] = &[("a b", "c d e")];

    #[test]
    fn matches_xl_vectors() {
        for &(ticks, case, expected) in VECTORS {
            let args = match case {
                "launch" => LAUNCH_ARGS,
                "spaced" => SPACED_ARGS,
                other => panic!("未知案例 {other}"),
            };
            assert_eq!(build_encrypted_args(ticks, args), expected, "ticks={ticks} case={case}");
        }
    }
}
