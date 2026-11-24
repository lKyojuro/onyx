//! Desktop Window Manager (DWM) backdrop, dark titlebar, and rounded corner integration.

use onyx_core::{OnyxError, OnyxResult};
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::Mutex;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWM_SYSTEMBACKDROP_TYPE, DWM_WINDOW_CORNER_PREFERENCE, DWMSBT_MAINWINDOW, DWMSBT_NONE,
    DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, GetSystemMetrics, HICON, ICON_BIG, ICON_SMALL, IMAGE_ICON, IsWindow,
    LR_DEFAULTCOLOR, LoadImageW, SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON, SMTO_ABORTIFHUNG,
    SMTO_ERRORONEXIT, SendMessageTimeoutW, WM_SETICON,
};
use windows::core::{BOOL, PCWSTR};

// WM_SETICON borrows the HICONs. Hold both handles for the lifetime of the executable.
static EXE_ICONS: Mutex<Option<[OwnedIcon; 2]>> = Mutex::new(None);
const EXE_ICON_RESOURCE_ID: usize = 1;

// Windows icon handles can be sent to other threads in the process as message values.
struct OwnedIcon(isize);

impl Drop for OwnedIcon {
    fn drop(&mut self) {
        // SAFETY: This is the owned, non-shared handle returned by LoadImageW.
        let _ = unsafe { DestroyIcon(HICON(self.0 as _)) };
    }
}

/// Sets the small and large window icons from the main executable's Onyx icon resource.
pub fn set_window_icon_from_exe(hwnd: isize) -> OnyxResult<()> {
    let window = crate::own_window(hwnd)?;
    let [small, big] = {
        let mut icons = EXE_ICONS.lock().map_err(|error| OnyxError::Platform {
            message: format!("Could not access the executable icon cache: {error}"),
        })?;
        let [small, big] = match icons.as_ref() {
            Some(pair) => pair,
            None => icons.insert(load_exe_icons().map_err(|error| OnyxError::Platform {
                message: format!("Could not load the executable icon: {error}"),
            })?),
        };
        [small.0, big.0]
    };

    for (kind, icon) in [(ICON_SMALL, small), (ICON_BIG, big)] {
        // SAFETY: WM_SETICON takes an icon handle retained in EXE_ICONS and does not
        // transfer ownership. No lock is held during the window procedure call.
        let sent = unsafe {
            SendMessageTimeoutW(
                window,
                WM_SETICON,
                WPARAM(kind as usize),
                LPARAM(icon),
                SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
                1000,
                None,
            )
        };
        if sent.0 == 0 {
            return Err(OnyxError::Platform {
                message: format!("Could not set icon type {kind} on HWND 0x{hwnd:X}"),
            });
        }
    }
    Ok(())
}

fn load_exe_icons() -> windows::core::Result<[OwnedIcon; 2]> {
    // SAFETY: A null name queries the current executable, whose module remains loaded.
    let module = unsafe { GetModuleHandleW(PCWSTR::null()) }?;
    let load = |width, height| {
        // SAFETY: The module is valid and this integer resource ID is encoded as
        // MAKEINTRESOURCEW. The returned non-shared icon is owned by OwnedIcon.
        unsafe {
            LoadImageW(
                Some(HINSTANCE(module.0)),
                PCWSTR(EXE_ICON_RESOURCE_ID as *const u16),
                IMAGE_ICON,
                GetSystemMetrics(width),
                GetSystemMetrics(height),
                LR_DEFAULTCOLOR,
            )
        }
        .map(|handle| OwnedIcon(handle.0 as isize))
    };
    Ok([load(SM_CXSMICON, SM_CYSMICON)?, load(SM_CXICON, SM_CYICON)?])
}

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
