//! Desktop Window Manager (DWM) backdrop, dark titlebar, and rounded corner integration.

use onyx_core::{OnyxError, OnyxResult};
use std::ffi::c_void;
use std::mem::size_of;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{
    DWM_SYSTEMBACKDROP_TYPE, DWM_WINDOW_CORNER_PREFERENCE, DWMSBT_MAINWINDOW, DWMSBT_NONE,
    DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::UI::WindowsAndMessaging::IsWindow;
use windows::core::BOOL;

/// Applies Windows 11 DWM visual attributes to a top-level window handle:
/// - `DWMWA_USE_IMMERSIVE_DARK_MODE` based on `dark_mode`
/// - `DWMWA_SYSTEMBACKDROP_TYPE` set to `DWMSBT_MAINWINDOW` (Mica) when `mica` is true, else `DWMSBT_NONE`
/// - `DWMWA_WINDOW_CORNER_PREFERENCE` set to `DWMWCP_ROUND`
///
/// Returns `Err` if `hwnd` is null or not a valid window handle.
/// On older Windows builds or hosts without DWM backdrop support, unsupported attribute
/// calls degrade gracefully and return `Ok(())`.
pub fn apply_window_backdrop(hwnd: isize, dark_mode: bool, mica: bool) -> OnyxResult<()> {
    if hwnd == 0 {
        return Err(OnyxError::Platform {
            message: "Cannot apply DWM backdrop to a null HWND".to_string(),
        });
    }

    let window = HWND(hwnd as *mut c_void);

    // SAFETY: `IsWindow` safely validates whether `window` refers to an existing Win32 window.
    let valid = unsafe { IsWindow(Some(window)) }.as_bool();
    if !valid {
        return Err(OnyxError::Platform {
            message: format!("HWND 0x{hwnd:X} is not a valid window handle"),
        });
    }

    let dark_value = BOOL::from(dark_mode);
    // SAFETY: `window` was validated by `IsWindow`, and `dark_value` is a stack-allocated `BOOL`
    // matching the required `sizeof(BOOL)` payload for `DWMWA_USE_IMMERSIVE_DARK_MODE`.
    if let Err(err) = unsafe {
        DwmSetWindowAttribute(
            window,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&raw const dark_value).cast::<c_void>(),
            size_of::<BOOL>() as u32,
        )
    } {
        tracing::debug!(
            ?err,
            "DWMWA_USE_IMMERSIVE_DARK_MODE not applied on this build"
        );
    }

    let backdrop: DWM_SYSTEMBACKDROP_TYPE = if mica { DWMSBT_MAINWINDOW } else { DWMSBT_NONE };
    // SAFETY: `window` is a valid HWND and `backdrop` matches `DWM_SYSTEMBACKDROP_TYPE` size.
    if let Err(err) = unsafe {
        DwmSetWindowAttribute(
            window,
            DWMWA_SYSTEMBACKDROP_TYPE,
            (&raw const backdrop).cast::<c_void>(),
            size_of::<DWM_SYSTEMBACKDROP_TYPE>() as u32,
        )
    } {
        tracing::debug!(
            ?err,
            mica,
            "DWMWA_SYSTEMBACKDROP_TYPE not supported on this host"
        );
    }

    let corner: DWM_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND;
    // SAFETY: `window` is a valid HWND and `corner` matches `DWM_WINDOW_CORNER_PREFERENCE` size.
    if let Err(err) = unsafe {
        DwmSetWindowAttribute(
            window,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&raw const corner).cast::<c_void>(),
            size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        )
    } {
        tracing::debug!(
            ?err,
            "DWMWA_WINDOW_CORNER_PREFERENCE not supported on this host"
        );
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn reject_null_and_invalid_hwnd() {
        assert!(apply_window_backdrop(0, true, true).is_err());
        assert!(apply_window_backdrop(0xDEAD_BEEF, true, false).is_err());
    }
}
