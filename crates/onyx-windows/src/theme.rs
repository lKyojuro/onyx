//! Windows system theme and accent color detection via the Registry.

use crate::to_wide_null;
use onyx_core::ThemeMode;
use std::ffi::{OsStr, c_void};
use std::mem::size_of;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::core::PCWSTR;

/// Reads a `REG_DWORD` value from `HKEY_CURRENT_USER`.
fn read_hkcu_dword(subkey: &str, value_name: &str) -> Option<u32> {
    let subkey_w = to_wide_null(OsStr::new(subkey));
    let value_w = to_wide_null(OsStr::new(value_name));
    let mut data: u32 = 0;
    let mut data_size = size_of::<u32>() as u32;

    // SAFETY: Both string pointers are valid null-terminated UTF-16 buffers, and `data`
    // is a properly aligned 4-byte `u32` buffer matching `data_size`.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey_w.as_ptr()),
            PCWSTR(value_w.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut data).cast::<c_void>()),
            Some(&raw mut data_size),
        )
    };

    if status.is_ok() { Some(data) } else { None }
}

/// Converts a Windows DWM `ABGR` (`0xAABBGGRR`) DWORD into an `#RRGGBB` hex string.
pub(crate) fn abgr_to_hex(abgr: u32) -> String {
    let r = (abgr & 0xFF) as u8;
    let g = ((abgr >> 8) & 0xFF) as u8;
    let b = ((abgr >> 16) & 0xFF) as u8;
    format!("#{r:02X}{g:02X}{b:02X}")
}

/// Detects the current Windows app theme (`Light` or `Dark`) from
/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\AppsUseLightTheme`.
///
/// Falls back to [`ThemeMode::Dark`] if the registry value is unavailable.
#[must_use]
pub fn detect_system_theme() -> ThemeMode {
    match read_hkcu_dword(
        r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        "AppsUseLightTheme",
    ) {
        Some(1) => ThemeMode::Light,
        Some(0) => ThemeMode::Dark,
        other => {
            tracing::debug!(
                ?other,
                "Could not read AppsUseLightTheme; defaulting to Dark"
            );
            ThemeMode::Dark
        }
    }
}

/// Detects the Windows DWM accent color from `HKCU\Software\Microsoft\Windows\DWM\AccentColor`
/// and formats it as `#RRGGBB`.
#[must_use]
pub fn detect_system_accent_hex() -> Option<String> {
    read_hkcu_dword(r"Software\Microsoft\Windows\DWM", "AccentColor").map(abgr_to_hex)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn abgr_conversion_produces_rrggbb() {
        // ABGR: A=0xFF, B=0xD4, G=0x78, R=0x00 -> #0078D4 (Windows blue)
        assert_eq!(abgr_to_hex(0xFF_D4_78_00), "#0078D4");
        assert_eq!(abgr_to_hex(0xFF_33_22_11), "#112233");
    }

    #[test]
    fn detect_theme_returns_concrete_mode() {
        let mode = detect_system_theme();
        assert!(matches!(mode, ThemeMode::Dark | ThemeMode::Light));
    }

    #[test]
    fn detect_accent_if_present_is_valid_hex() {
        if let Some(hex) = detect_system_accent_hex() {
            assert_eq!(hex.len(), 7);
            assert!(hex.starts_with('#'));
            assert!(hex[1..].chars().all(|c| c.is_ascii_hexdigit()));
        }
    }
}
