//! Isolated Win32, COM, DWM, and Windows Shell integration for Onyx.
//!
//! All `unsafe` Win32/COM/Shell calls in the workspace live strictly inside this crate,
//! wrapped with safe Rust APIs, explicit `// SAFETY:` comments, and RAII resource guards.

pub mod clipboard;
pub mod com;
pub mod drives;
pub mod dwm;
pub mod icons;
pub mod known_folders;
pub mod shell;
pub mod theme;
pub mod usn;

pub use clipboard::{get_clipboard_files, set_clipboard_files};
pub use com::ComGuard;
pub use drives::{enumerate_drives, free_disk_space};
pub use dwm::apply_window_backdrop;
pub use icons::{RgbaIcon, extract_file_icon_rgba};
pub use known_folders::enumerate_known_folders;
pub use shell::{
    launch_terminal, open_file, open_with_dialog, recycle_paths, show_properties_dialog,
};
pub use theme::{detect_system_accent_hex, detect_system_theme};
pub use usn::{UsnCapability, probe_ntfs_usn_capability};

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

/// Encodes an `OsStr` into a null-terminated UTF-16 buffer suitable for Win32 `*W` APIs.
pub(crate) fn to_wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

/// Decodes a null-terminated UTF-16 slice into a `String`, stopping at the first `0`.
pub(crate) fn wide_slice_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}
