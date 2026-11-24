//! Windows Clipboard integration for file lists (`CF_HDROP` + `"Preferred DropEffect"`).

use crate::to_wide_null;
use onyx_core::{OnyxError, OnyxResult, normalize_display_path};
use std::ffi::{OsStr, OsString, c_void};
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr;
use std::time::Duration;
use windows::Win32::Foundation::{FALSE, GlobalFree, HANDLE, HGLOBAL, POINT, TRUE};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::UI::Shell::DROPFILES;
use windows::core::PCWSTR;

const CF_HDROP: u32 = 15;
const DROPEFFECT_COPY: u32 = 1;
const DROPEFFECT_MOVE: u32 = 2;
const PREFERRED_DROP_EFFECT: &str = "Preferred DropEffect";

/// RAII guard for an open Windows clipboard session.
struct ClipboardGuard;

impl ClipboardGuard {
    fn open() -> OnyxResult<Self> {
        let mut last_err = None;
        for _ in 0..8 {
            // SAFETY: `OpenClipboard(None)` opens the clipboard associated with the current task.
            match unsafe { OpenClipboard(None) } {
                Ok(()) => return Ok(Self),
                Err(err) => {
                    last_err = Some(err);
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        Err(OnyxError::Platform {
            message: format!("OpenClipboard failed: {:?}", last_err),
        })
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: `ClipboardGuard` is constructed only after a successful `OpenClipboard`.
        let _ = unsafe { CloseClipboard() };
    }
}

/// RAII guard for a `GlobalAlloc` memory block that frees on drop unless transferred to the clipboard.
struct GlobalMemGuard {
    handle: HGLOBAL,
    owned: bool,
}

impl GlobalMemGuard {
    fn alloc_zeroed(bytes: usize) -> OnyxResult<Self> {
        // SAFETY: `GlobalAlloc` with `GMEM_MOVEABLE | GMEM_ZEROINIT` returns a valid movable HGLOBAL.
        let handle =
            unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes) }.map_err(|err| {
                OnyxError::Platform {
                    message: format!("GlobalAlloc({bytes}) failed: {err}"),
                }
            })?;
        Ok(Self {
            handle,
            owned: true,
        })
    }

    fn handle(&self) -> HGLOBAL {
        self.handle
    }

    /// Transfers ownership of the `HGLOBAL` (e.g., after `SetClipboardData` succeeds).
    fn release_ownership(&mut self) {
        self.owned = false;
    }
}

impl Drop for GlobalMemGuard {
    fn drop(&mut self) {
        if self.owned && !self.handle.is_invalid() {
            // SAFETY: `self.handle` is still owned by us (never handed to the clipboard) and freed once.
            let _ = unsafe { GlobalFree(Some(self.handle)) };
        }
    }
}

/// RAII guard for `GlobalLock` / `GlobalUnlock`.
struct GlobalLockGuard {
    handle: HGLOBAL,
    ptr: *mut c_void,
}

impl GlobalLockGuard {
    fn lock(handle: HGLOBAL) -> Option<Self> {
        // SAFETY: `handle` is a valid movable `HGLOBAL`.
        let ptr = unsafe { GlobalLock(handle) };
        if ptr.is_null() {
            None
        } else {
            Some(Self { handle, ptr })
        }
    }

    fn as_ptr(&self) -> *const u8 {
        self.ptr.cast::<u8>()
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.cast::<u8>()
    }
}

impl Drop for GlobalLockGuard {
    fn drop(&mut self) {
        // SAFETY: Paired strictly with a successful `GlobalLock` in `Self::lock`.
        let _ = unsafe { GlobalUnlock(self.handle) };
    }
}

fn preferred_drop_effect_format() -> OnyxResult<u32> {
    let wide = to_wide_null(OsStr::new(PREFERRED_DROP_EFFECT));
    // SAFETY: `wide` is a valid null-terminated UTF-16 format name.
    let fmt = unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) };
    if fmt == 0 {
        Err(OnyxError::Platform {
            message: "RegisterClipboardFormatW(\"Preferred DropEffect\") failed".to_string(),
        })
    } else {
        Ok(fmt)
    }
}

/// Writes `paths` to the Windows clipboard using `CF_HDROP` (`DROPFILES` UTF-16 double-null list)
/// and sets `"Preferred DropEffect"` to `DROPEFFECT_MOVE` (when `is_cut`) or `DROPEFFECT_COPY`.
pub fn set_clipboard_files(paths: &[PathBuf], is_cut: bool) -> OnyxResult<()> {
    if paths.is_empty() {
        return Err(OnyxError::InvalidPath {
            message: "Cannot copy an empty file list to the clipboard".to_string(),
        });
    }

    // Build double-null-terminated UTF-16 path list.
    let mut wide_list: Vec<u16> = Vec::new();
    for path in paths {
        let display = normalize_display_path(path);
        wide_list.extend(OsStr::new(&display).encode_wide());
        wide_list.push(0);
    }
    wide_list.push(0);

    let header = DROPFILES {
        pFiles: size_of::<DROPFILES>() as u32,
        pt: POINT { x: 0, y: 0 },
        fNC: FALSE,
        fWide: TRUE,
    };

    let drop_bytes = size_of::<DROPFILES>() + wide_list.len() * size_of::<u16>();
    let mut drop_mem = GlobalMemGuard::alloc_zeroed(drop_bytes)?;
    {
        let mut lock =
            GlobalLockGuard::lock(drop_mem.handle()).ok_or_else(|| OnyxError::Platform {
                message: "GlobalLock failed for CF_HDROP payload".to_string(),
            })?;
        // SAFETY: `drop_mem` was allocated with `drop_bytes` bytes; header and UTF-16 list fit strictly inside.
        unsafe {
            ptr::copy_nonoverlapping(
                (&raw const header).cast::<u8>(),
                lock.as_mut_ptr(),
                size_of::<DROPFILES>(),
            );
            ptr::copy_nonoverlapping(
                wide_list.as_ptr().cast::<u8>(),
                lock.as_mut_ptr().add(size_of::<DROPFILES>()),
                wide_list.len() * size_of::<u16>(),
            );
        }
    }

    let effect: u32 = if is_cut {
        DROPEFFECT_MOVE
    } else {
        DROPEFFECT_COPY
    };
    let mut effect_mem = GlobalMemGuard::alloc_zeroed(size_of::<u32>())?;
    {
        let mut lock =
            GlobalLockGuard::lock(effect_mem.handle()).ok_or_else(|| OnyxError::Platform {
                message: "GlobalLock failed for Preferred DropEffect payload".to_string(),
            })?;
        // SAFETY: `effect_mem` has `size_of::<u32>()` bytes.
        unsafe {
            ptr::copy_nonoverlapping(
                (&raw const effect).cast::<u8>(),
                lock.as_mut_ptr(),
                size_of::<u32>(),
            );
        }
    }

    let drop_effect_fmt = preferred_drop_effect_format()?;
    let _clip = ClipboardGuard::open()?;

    // SAFETY: Clipboard is open via `_clip`.
    unsafe { EmptyClipboard() }.map_err(|err| OnyxError::Platform {
        message: format!("EmptyClipboard failed: {err}"),
    })?;

    // SAFETY: Clipboard is open and `drop_mem` is unlocked; on success, Windows takes ownership of `HGLOBAL`.
    unsafe { SetClipboardData(CF_HDROP, Some(HANDLE(drop_mem.handle().0))) }.map_err(|err| {
        OnyxError::Platform {
            message: format!("SetClipboardData(CF_HDROP) failed: {err}"),
        }
    })?;
    drop_mem.release_ownership();

    // SAFETY: Clipboard is open and `effect_mem` is unlocked; on success, Windows takes ownership of `HGLOBAL`.
    unsafe { SetClipboardData(drop_effect_fmt, Some(HANDLE(effect_mem.handle().0))) }.map_err(
        |err| OnyxError::Platform {
            message: format!("SetClipboardData(Preferred DropEffect) failed: {err}"),
        },
    )?;
    effect_mem.release_ownership();

    Ok(())
}

/// Reads file paths from the Windows clipboard (`CF_HDROP`) along with whether the clipboard
/// carries a cut (`DROPEFFECT_MOVE`) flag via `"Preferred DropEffect"`.
#[must_use]
pub fn get_clipboard_files() -> Option<(Vec<PathBuf>, bool)> {
    let _clip = ClipboardGuard::open().ok()?;

    // SAFETY: Clipboard is open; returned handle is owned by the clipboard (do not free).
    let raw_handle = unsafe { GetClipboardData(CF_HDROP) }.ok()?;
    let hglobal = HGLOBAL(raw_handle.0);
    if hglobal.is_invalid() {
        return None;
    }

    // SAFETY: `hglobal` is a valid clipboard global memory handle while clipboard is open.
    let total_size = unsafe { GlobalSize(hglobal) };
    if total_size < size_of::<DROPFILES>() {
        return None;
    }

    let lock = GlobalLockGuard::lock(hglobal)?;
    // SAFETY: `total_size >= size_of::<DROPFILES>()`, so unaligned read of `DROPFILES` is in-bounds.
    let header: DROPFILES = unsafe { ptr::read_unaligned(lock.as_ptr().cast::<DROPFILES>()) };
    let offset = header.pFiles as usize;
    if offset >= total_size {
        return None;
    }

    let payload_len = total_size - offset;
    // SAFETY: `offset < total_size` and `offset + payload_len == total_size`.
    let payload = unsafe { std::slice::from_raw_parts(lock.as_ptr().add(offset), payload_len) };

    let mut paths = Vec::new();
    if header.fWide.as_bool() {
        let u16_count = payload.len() / 2;
        let mut units = Vec::with_capacity(u16_count);
        for pair in payload.chunks_exact(2) {
            units.push(u16::from_le_bytes([pair[0], pair[1]]));
        }
        let mut start = 0usize;
        while start < units.len() {
            let rel_end = units[start..]
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(units.len() - start);
            if rel_end == 0 {
                break;
            }
            let os = OsString::from_wide(&units[start..start + rel_end]);
            paths.push(PathBuf::from(os));
            start += rel_end + 1;
        }
    } else {
        let mut start = 0usize;
        while start < payload.len() {
            let rel_end = payload[start..]
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(payload.len() - start);
            if rel_end == 0 {
                break;
            }
            let s = String::from_utf8_lossy(&payload[start..start + rel_end]);
            paths.push(PathBuf::from(s.into_owned()));
            start += rel_end + 1;
        }
    }

    drop(lock);

    if paths.is_empty() {
        return None;
    }

    let is_cut = read_preferred_drop_effect().is_some_and(|effect| (effect & DROPEFFECT_MOVE) != 0);
    Some((paths, is_cut))
}

fn read_preferred_drop_effect() -> Option<u32> {
    let fmt = preferred_drop_effect_format().ok()?;
    // SAFETY: Caller holds an active `ClipboardGuard`.
    let raw = unsafe { GetClipboardData(fmt) }.ok()?;
    let hglobal = HGLOBAL(raw.0);
    if hglobal.is_invalid() {
        return None;
    }
    // SAFETY: `hglobal` is valid while the clipboard is open.
    if unsafe { GlobalSize(hglobal) } < size_of::<u32>() {
        return None;
    }
    let lock = GlobalLockGuard::lock(hglobal)?;
    // SAFETY: Buffer size verified to be at least 4 bytes.
    let value = unsafe { ptr::read_unaligned(lock.as_ptr().cast::<u32>()) };
    Some(value)
}

use std::os::windows::ffi::OsStrExt;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialize clipboard tests so they never race with each other on the global Windows clipboard.
    static CLIPBOARD_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn set_empty_paths_returns_error() {
        assert!(matches!(
            set_clipboard_files(&[], false),
            Err(OnyxError::InvalidPath { .. })
        ));
    }

    #[test]
    fn roundtrip_copy_and_cut_file_lists() {
        let _guard = CLIPBOARD_TEST_LOCK.lock().unwrap();

        let paths = vec![
            PathBuf::from(r"C:\Temp\alpha.txt"),
            PathBuf::from(r"C:\Temp\Folder With Spaces\beta.rs"),
        ];

        set_clipboard_files(&paths, false).expect("set clipboard copy should succeed");
        let (read_paths, is_cut) =
            get_clipboard_files().expect("clipboard should contain CF_HDROP paths");
        assert_eq!(read_paths, paths);
        assert!(!is_cut);

        set_clipboard_files(&paths, true).expect("set clipboard cut should succeed");
        let (read_paths_cut, is_cut_flag) =
            get_clipboard_files().expect("clipboard should contain CF_HDROP cut paths");
        assert_eq!(read_paths_cut, paths);
        assert!(is_cut_flag);
    }
}
