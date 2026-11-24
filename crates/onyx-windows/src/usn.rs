//! NTFS USN Change Journal capability probing via `DeviceIoControl(FSCTL_QUERY_USN_JOURNAL)`.

use crate::{to_wide_null, wide_slice_to_string};
use onyx_core::normalize_display_path;
use std::ffi::{OsStr, c_void};
use std::mem::size_of;
use std::path::Path;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_ELEVATION_REQUIRED, ERROR_INVALID_FUNCTION,
    ERROR_JOURNAL_NOT_ACTIVE, ERROR_NOT_SUPPORTED, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
    MAX_PATH, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetVolumeInformationW, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{FSCTL_QUERY_USN_JOURNAL, USN_JOURNAL_DATA_V0};
use windows::core::PCWSTR;

/// Result of probing NTFS USN Change Journal capability for a volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsnCapability {
    /// Volume supports NTFS USN journal and returned active journal metadata.
    Supported { journal_id: u64, next_usn: i64 },
    /// Volume is NTFS, but opening/querying the journal requires Administrator elevation.
    RequiresElevation,
    /// Volume uses a non-NTFS filesystem (e.g., FAT32, exFAT, ReFS).
    UnsupportedFilesystem(String),
    /// Journal could not be queried (e.g., journal inactive, UNC path, or I/O error).
    Unavailable(String),
}

/// RAII guard for a Win32 kernel `HANDLE`.
struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: `self.0` is an owned Win32 handle opened by `CreateFileW`.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// Extracts `(drive_root, volume_device_path)` (e.g., `("C:\\", "\\\\.\\C:")`) from a Windows path.
fn extract_drive_volume(path: &Path) -> Option<(String, String)> {
    let display = normalize_display_path(path);
    let mut chars = display.chars();
    let letter = chars.next()?;
    let colon = chars.next()?;
    if letter.is_ascii_alphabetic() && colon == ':' {
        let upper = letter.to_ascii_uppercase();
        Some((format!("{upper}:\\"), format!(r"\\.\{upper}:")))
    } else {
        None
    }
}

fn query_filesystem_name(root: &str) -> Result<String, String> {
    let wide = to_wide_null(OsStr::new(root));
    let mut fs_buf = vec![0u16; (MAX_PATH + 1) as usize];

    // SAFETY: `wide` is null-terminated UTF-16 and `fs_buf` is a writable UTF-16 buffer.
    unsafe {
        GetVolumeInformationW(
            PCWSTR(wide.as_ptr()),
            None,
            None,
            None,
            None,
            Some(&mut fs_buf),
        )
    }
    .map_err(|err| format!("GetVolumeInformationW failed for '{root}': {err}"))?;

    Ok(wide_slice_to_string(&fs_buf))
}

/// Probes whether the volume hosting `path` supports querying the NTFS USN Change Journal.
#[must_use]
pub fn probe_ntfs_usn_capability(path: &Path) -> UsnCapability {
    let Some((drive_root, device_path)) = extract_drive_volume(path) else {
        return UsnCapability::Unavailable(format!(
            "Path '{}' is not anchored to a local drive letter",
            path.display()
        ));
    };

    let fs_name = match query_filesystem_name(&drive_root) {
        Ok(name) => name,
        Err(msg) => return UsnCapability::Unavailable(msg),
    };

    if !fs_name.eq_ignore_ascii_case("NTFS") {
        return UsnCapability::UnsupportedFilesystem(fs_name);
    }

    let device_w = to_wide_null(OsStr::new(&device_path));
    // SAFETY: `device_w` is a null-terminated UTF-16 volume device path; returned handle is wrapped in `HandleGuard`.
    let handle_result = unsafe {
        CreateFileW(
            PCWSTR(device_w.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    };

    let handle = match handle_result {
        Ok(h) if !h.is_invalid() && h != INVALID_HANDLE_VALUE => HandleGuard(h),
        Ok(_) => {
            return UsnCapability::Unavailable(format!(
                "CreateFileW returned an invalid handle for '{device_path}'"
            ));
        }
        Err(err) => {
            let code = WIN32_ERROR::from_error(&err);
            if code == Some(ERROR_ACCESS_DENIED) || code == Some(ERROR_ELEVATION_REQUIRED) {
                return UsnCapability::RequiresElevation;
            }
            return UsnCapability::Unavailable(format!(
                "Failed to open volume '{device_path}': {err}"
            ));
        }
    };

    let mut journal_data = USN_JOURNAL_DATA_V0::default();
    let mut bytes_returned: u32 = 0;

    // SAFETY: `handle.0` is an open volume handle, and `journal_data` is a stack-allocated
    // `USN_JOURNAL_DATA_V0` output buffer whose byte size is passed accurately.
    let ioctl_result = unsafe {
        DeviceIoControl(
            handle.0,
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            Some((&raw mut journal_data).cast::<c_void>()),
            size_of::<USN_JOURNAL_DATA_V0>() as u32,
            Some(&raw mut bytes_returned),
            None,
        )
    };

    match ioctl_result {
        Ok(()) => UsnCapability::Supported {
            journal_id: journal_data.UsnJournalID,
            next_usn: journal_data.NextUsn,
        },
        Err(err) => {
            let code = WIN32_ERROR::from_error(&err);
            if code == Some(ERROR_ACCESS_DENIED) || code == Some(ERROR_ELEVATION_REQUIRED) {
                UsnCapability::RequiresElevation
            } else if code == Some(ERROR_JOURNAL_NOT_ACTIVE) {
                UsnCapability::Unavailable(format!(
                    "USN journal is not active on volume '{drive_root}'"
                ))
            } else if code == Some(ERROR_NOT_SUPPORTED) || code == Some(ERROR_INVALID_FUNCTION) {
                UsnCapability::UnsupportedFilesystem(fs_name)
            } else {
                UsnCapability::Unavailable(format!(
                    "FSCTL_QUERY_USN_JOURNAL failed on '{drive_root}': {err}"
                ))
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn extract_drive_volume_handles_standard_and_extended_paths() {
        assert_eq!(
            extract_drive_volume(Path::new(r"c:\Users\Test")),
            Some(("C:\\".to_string(), r"\\.\C:".to_string()))
        );
        assert_eq!(
            extract_drive_volume(Path::new(r"\\?\D:\Onyx")),
            Some(("D:\\".to_string(), r"\\.\D:".to_string()))
        );
        assert_eq!(extract_drive_volume(Path::new(r"\\server\share\dir")), None);
    }

    #[test]
    fn probe_usn_on_temp_dir_returns_structured_result() {
        let temp = tempfile::TempDir::new().unwrap();
        let cap = probe_ntfs_usn_capability(temp.path());
        match cap {
            UsnCapability::Supported { .. }
            | UsnCapability::RequiresElevation
            | UsnCapability::UnsupportedFilesystem(_)
            | UsnCapability::Unavailable(_) => {}
        }
    }
}
