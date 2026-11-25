//! Isolated Win32, COM, DWM, and Windows Shell integration for Onyx.
//!
//! All `unsafe` Win32/COM/Shell calls in the workspace live strictly inside this crate,
//! wrapped with safe Rust APIs, explicit `// SAFETY:` comments, and RAII resource guards.

pub mod clipboard;
pub mod com;
mod console;
pub mod context_menu;
pub mod drives;
pub mod dwm;
pub mod icons;
pub mod known_folders;
pub mod mouse_nav;
pub mod shell;
pub mod theme;
pub mod usn;

pub use clipboard::{get_clipboard_files, set_clipboard_files};
pub use com::ComGuard;
pub use console::attach_parent_console;
pub use context_menu::show_context_menu;
pub use drives::{enumerate_drives, free_disk_space};
pub use dwm::{
    apply_titlebar_colors, apply_window_backdrop, has_system_font_family, set_window_icon_from_exe,
};
pub use icons::{
    IconSize, RgbaIcon, extract_file_icon_rgba, extract_file_icon_rgba_sized,
    extract_path_icon_rgba, extract_path_icon_rgba_sized,
};
pub use known_folders::enumerate_known_folders;
pub use mouse_nav::{
    MouseNavDirection, MouseNavHook, install_mouse_nav_hook, uninstall_mouse_nav_hook,
};
pub use shell::{
    TerminalProfile, launch_terminal, launch_terminal_with_profile, open_file, open_with_dialog,
    recycle_paths, show_properties_dialog,
};
pub use theme::{detect_system_accent_hex, detect_system_theme};
pub use usn::{UsnCapability, probe_ntfs_usn_capability};

use onyx_core::{OnyxError, OnyxResult};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

fn own_window(hwnd: isize) -> OnyxResult<HWND> {
    let window = HWND(hwnd as _);
    let mut process = 0;
    // SAFETY: The query accepts any HWND value and writes to a valid process ID output.
    let (thread, own_process) = unsafe {
        (
            GetWindowThreadProcessId(window, Some(&raw mut process)),
            GetCurrentProcessId(),
        )
    };
    if thread == 0 || process != own_process {
        return Err(OnyxError::Platform {
            message: format!("HWND 0x{hwnd:X} is not a window in the current process"),
        });
    }
    Ok(window)
}

/// Encodes an `OsStr` into a null-terminated UTF-16 buffer suitable for Win32 `*W` APIs.
pub(crate) fn to_wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

/// Decodes a null-terminated UTF-16 slice into a `String`, stopping at the first `0`.
pub(crate) fn wide_slice_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}
