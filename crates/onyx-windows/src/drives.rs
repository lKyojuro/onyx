//! Logical drive enumeration and disk space queries via Win32 storage APIs.

use crate::{to_wide_null, wide_slice_to_string};
use onyx_core::{DriveInfo, DriveKind};
use std::path::{Path, PathBuf};
use windows::Win32::Foundation::MAX_PATH;
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDriveStringsW, GetVolumeInformationW,
};
use windows::core::PCWSTR;

const DRIVE_REMOVABLE_TYPE: u32 = 2;
const DRIVE_FIXED_TYPE: u32 = 3;
const DRIVE_REMOTE_TYPE: u32 = 4;
const DRIVE_CDROM_TYPE: u32 = 5;
const DRIVE_RAMDISK_TYPE: u32 = 6;

fn map_drive_kind(drive_type: u32) -> DriveKind {
    match drive_type {
        DRIVE_FIXED_TYPE => DriveKind::Fixed,
        DRIVE_REMOVABLE_TYPE => DriveKind::Removable,
        DRIVE_REMOTE_TYPE => DriveKind::Network,
        DRIVE_CDROM_TYPE => DriveKind::CdRom,
        DRIVE_RAMDISK_TYPE => DriveKind::RamDisk,
        _ => DriveKind::Unknown,
    }
}

/// Enumerates all mounted logical drives on Windows along with volume label, filesystem,
/// drive kind, total bytes, and available free bytes.
#[must_use]
pub fn enumerate_drives() -> Vec<DriveInfo> {
    // SAFETY: Passing `None` queries the required buffer size in UTF-16 code units.
    let required = unsafe { GetLogicalDriveStringsW(None) };
    if required == 0 {
        return Vec::new();
    }

    let mut buffer = vec![0u16; required as usize + 1];
    // SAFETY: `buffer` is a valid mutable slice large enough to hold the drive strings.
    let written = unsafe { GetLogicalDriveStringsW(Some(&mut buffer)) };
    if written == 0 {
        return Vec::new();
    }

    let mut drives = Vec::new();
    let valid_slice = &buffer[..(written as usize).min(buffer.len())];

    for root_slice in valid_slice.split(|&ch| ch == 0) {
        if root_slice.is_empty() {
            continue;
        }

        let root_str = String::from_utf16_lossy(root_slice);
        let mut root_wide = Vec::with_capacity(root_slice.len() + 1);
        root_wide.extend_from_slice(root_slice);
        root_wide.push(0);
        let root_pcwstr = PCWSTR(root_wide.as_ptr());

        // SAFETY: `root_pcwstr` points to a valid null-terminated UTF-16 drive root path.
        let kind = map_drive_kind(unsafe { GetDriveTypeW(root_pcwstr) });

        let mut volume_name = vec![0u16; (MAX_PATH + 1) as usize];
        let mut fs_name = vec![0u16; (MAX_PATH + 1) as usize];

        // SAFETY: `root_pcwstr` is null-terminated; output slices are writable UTF-16 buffers.
        let vol_ok = unsafe {
            GetVolumeInformationW(
                root_pcwstr,
                Some(&mut volume_name),
                None,
                None,
                None,
                Some(&mut fs_name),
            )
        }
        .is_ok();

        let (label, filesystem) = if vol_ok {
            (
                wide_slice_to_string(&volume_name),
                wide_slice_to_string(&fs_name),
            )
        } else {
            (String::new(), String::new())
        };

        let mut free_available = 0u64;
        let mut total_bytes = 0u64;
        // SAFETY: `root_pcwstr` is null-terminated and pointers target stack-allocated `u64`s.
        let space_ok = unsafe {
            GetDiskFreeSpaceExW(
                root_pcwstr,
                Some(&raw mut free_available),
                Some(&raw mut total_bytes),
                None,
            )
        }
        .is_ok();

        if !space_ok {
            free_available = 0;
            total_bytes = 0;
        }

        drives.push(DriveInfo {
            root: PathBuf::from(root_str),
            label,
            filesystem,
            kind,
            total_bytes,
            free_bytes: free_available,
        });
    }

    drives
}

/// Returns the available free disk space (in bytes, respecting user quotas) on the volume
/// hosting `path`.
///
/// If `path` does not yet exist on disk, walks up its ancestors until an existing directory
/// is located.
#[must_use]
pub fn free_disk_space(path: &Path) -> Option<u64> {
    let target_dir = if path.is_dir() {
        path
    } else {
        path.ancestors().find(|ancestor| ancestor.is_dir())?
    };

    let wide = to_wide_null(target_dir.as_os_str());
    let mut free_available = 0u64;

    // SAFETY: `wide` is null-terminated UTF-16 and `free_available` is a valid mutable `u64`.
    let result = unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            Some(&raw mut free_available),
            None,
            None,
        )
    };

    result.ok().map(|()| free_available)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn enumerate_drives_finds_at_least_one_drive() {
        let drives = enumerate_drives();
        assert!(
            !drives.is_empty(),
            "Windows host should report at least one logical drive"
        );
        assert!(drives.iter().any(|d| d.total_bytes > 0));
    }

    #[test]
    fn free_disk_space_works_for_existing_and_nonexistent_child() {
        let temp = tempfile::TempDir::new().unwrap();
        let space_existing = free_disk_space(temp.path());
        assert!(space_existing.is_some());

        let nested = temp.path().join("does_not_exist_yet").join("sub");
        let space_nested = free_disk_space(&nested);
        assert!(space_nested.is_some());
    }
}
