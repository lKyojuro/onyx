//! Zero-lag fast low-priority directory scanning and disk seek penalty probing.

use crate::to_wide_null;
use crate::transfer::{StorageMediaKind, detect_storage_media};
use onyx_core::{OnyxError, OnyxResult, to_extended_length_path};
use std::ffi::{OsString, c_void};
use std::marker::PhantomData;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES, ERROR_PATH_NOT_FOUND, HANDLE,
    INVALID_HANDLE_VALUE, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FIND_FIRST_EX_LARGE_FETCH, FindClose,
    FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW, WIN32_FIND_DATAW,
};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN, THREAD_MODE_BACKGROUND_END,
};
use windows::core::PCWSTR;

const WINDOWS_TO_UNIX_EPOCH_SECS: i64 = 11_644_473_600;

/// RAII guard that lowers thread I/O and CPU scheduling priority to background mode.
#[derive(Debug)]
pub struct BackgroundPriorityGuard {
    entered: bool,
    _marker: PhantomData<*mut ()>,
}

impl BackgroundPriorityGuard {
    /// Enters background priority mode on the current thread.
    #[must_use]
    pub fn enter() -> Self {
        // SAFETY: GetCurrentThread() returns a pseudo-handle for the calling thread.
        // SetThreadPriority with THREAD_MODE_BACKGROUND_BEGIN lowers CPU and I/O priority.
        let entered =
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) }.is_ok();

        Self {
            entered,
            _marker: PhantomData,
        }
    }
}

impl Drop for BackgroundPriorityGuard {
    fn drop(&mut self) {
        if self.entered {
            // SAFETY: Restores thread priority back to normal on the calling thread.
            let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END) };
        }
    }
}

/// Directory entry information extracted directly from `WIN32_FIND_DATAW` with zero extra syscalls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FastDirEntry {
    pub name: OsString,
    pub size: u64,
    pub is_dir: bool,
    pub is_reparse_point: bool,
    pub mtime_secs: Option<i64>,
}

/// RAII guard for `FindClose`.
#[derive(Debug)]
struct FindHandleGuard(HANDLE);

impl Drop for FindHandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: `self.0` is a valid search handle returned by `FindFirstFileExW`.
            let _ = unsafe { FindClose(self.0) };
        }
    }
}

/// Fast directory scan using `FindFirstFileExW(..., FindExInfoBasic, FIND_FIRST_EX_LARGE_FETCH)`.
///
/// Returns all entries within `dir` with zero extra `stat` / `symlink_metadata` syscalls.
pub fn scan_dir_entries(dir: &Path) -> OnyxResult<Vec<FastDirEntry>> {
    let extended_dir = to_extended_length_path(dir);
    let mut pattern_os = extended_dir.into_os_string();
    let pattern_str = pattern_os.to_string_lossy();
    if !pattern_str.ends_with('\\') && !pattern_str.ends_with('/') {
        pattern_os.push(r"\*");
    } else {
        pattern_os.push("*");
    }

    let pattern_wide = to_wide_null(&pattern_os);
    let mut find_data = WIN32_FIND_DATAW::default();

    // SAFETY: pattern_wide is null-terminated UTF-16, find_data is a valid mutable pointer.
    let handle = unsafe {
        FindFirstFileExW(
            PCWSTR(pattern_wide.as_ptr()),
            FindExInfoBasic,
            (&raw mut find_data).cast::<c_void>(),
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };

    let handle = match handle {
        Ok(h) => h,
        Err(err) => {
            let win32 = WIN32_ERROR::from_error(&err);
            if win32 == Some(ERROR_FILE_NOT_FOUND) || win32 == Some(ERROR_PATH_NOT_FOUND) {
                return Err(OnyxError::NotFound {
                    path: dir.to_path_buf(),
                });
            }
            if win32 == Some(ERROR_ACCESS_DENIED) {
                return Err(OnyxError::PermissionDenied {
                    path: dir.to_path_buf(),
                });
            }
            return Err(OnyxError::Platform {
                message: format!("FindFirstFileExW failed for {}: {err}", dir.display()),
            });
        }
    };

    let _guard = FindHandleGuard(handle);
    let mut entries = Vec::new();

    loop {
        // Skip "." and ".." without OsString/String allocation
        let is_dot = find_data.cFileName[0] == b'.' as u16
            && (find_data.cFileName[1] == 0
                || (find_data.cFileName[1] == b'.' as u16 && find_data.cFileName[2] == 0));

        if !is_dot {
            let len = find_data
                .cFileName
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(find_data.cFileName.len());
            let name = OsString::from_wide(&find_data.cFileName[..len]);
            let attrs = find_data.dwFileAttributes;
            let is_dir = (attrs & FILE_ATTRIBUTE_DIRECTORY.0) != 0;
            let is_reparse_point = (attrs & FILE_ATTRIBUTE_REPARSE_POINT.0) != 0;
            let size = ((find_data.nFileSizeHigh as u64) << 32) | (find_data.nFileSizeLow as u64);
            let ticks = ((find_data.ftLastWriteTime.dwHighDateTime as u64) << 32)
                | (find_data.ftLastWriteTime.dwLowDateTime as u64);
            let mtime_secs = if ticks == 0 {
                None
            } else {
                Some(((ticks / 10_000_000) as i64) - WINDOWS_TO_UNIX_EPOCH_SECS)
            };

            entries.push(FastDirEntry {
                name,
                size,
                is_dir,
                is_reparse_point,
                mtime_secs,
            });
        }

        // SAFETY: handle is valid, find_data is a valid mutable pointer.
        let next_res = unsafe { FindNextFileW(handle, &raw mut find_data) };
        match next_res {
            Ok(()) => {}
            Err(err) => {
                if WIN32_ERROR::from_error(&err) == Some(ERROR_NO_MORE_FILES) {
                    break;
                }
                return Err(OnyxError::Platform {
                    message: format!("FindNextFileW failed for {}: {err}", dir.display()),
                });
            }
        }
    }

    Ok(entries)
}

/// Recommends parallel worker count for scanning `path`.
///
/// Probes `StorageDeviceSeekPenaltyProperty` (cached per drive letter via [`detect_storage_media`]):
/// - 1 for HDD (avoids head thrashing)
/// - 4..=6 for SSD
/// - 2 for Unknown / Network / UNC
#[must_use]
pub fn recommend_scan_workers(path: &Path) -> usize {
    match detect_storage_media(path) {
        StorageMediaKind::Hdd => 1,
        StorageMediaKind::Ssd => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(4, 6),
        StorageMediaKind::Unknown => 2,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn background_priority_guard_enters_and_drops() {
        let guard = BackgroundPriorityGuard::enter();
        assert!(format!("{guard:?}").contains("BackgroundPriorityGuard"));
        drop(guard);
    }

    #[test]
    fn scan_dir_entries_finds_files_and_dirs() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("child_dir");
        fs::create_dir(&sub).unwrap();
        fs::write(tmp.path().join("hello.txt"), b"12345").unwrap();

        let entries = scan_dir_entries(tmp.path()).unwrap();
        assert_eq!(entries.len(), 2);

        let file = entries.iter().find(|e| e.name == "hello.txt").unwrap();
        assert_eq!(file.size, 5);
        assert!(!file.is_dir);
        assert!(!file.is_reparse_point);
        assert!(file.mtime_secs.is_some());

        let dir = entries.iter().find(|e| e.name == "child_dir").unwrap();
        assert!(dir.is_dir);
        assert!(!dir.is_reparse_point);
    }

    #[test]
    fn scan_dir_entries_handles_not_found() {
        let tmp = TempDir::new().unwrap();
        let nonexistent = tmp.path().join("does_not_exist");
        let res = scan_dir_entries(&nonexistent);
        assert!(matches!(res, Err(OnyxError::NotFound { .. })));
    }

    #[test]
    fn recommend_scan_workers_returns_valid_range() {
        let tmp = TempDir::new().unwrap();
        let workers = recommend_scan_workers(tmp.path());
        assert!((1..=6).contains(&workers));

        // Network / UNC paths return 2
        let unc_workers = recommend_scan_workers(Path::new(r"\\server\share\folder"));
        assert_eq!(unc_workers, 2);
    }
}
