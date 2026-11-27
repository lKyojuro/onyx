//! Native Windows shell file and folder icon extraction into top-down 32-bit RGBA pixel buffers.

use crate::com::ComGuard;
use crate::to_wide_null;
use std::ffi::c_void;
use std::mem::size_of;
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
    GetObjectW, HBITMAP, HDC, HGDIOBJ, ReleaseDC,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES,
};
use windows::Win32::UI::Shell::{
    SHFILEINFOW, SHGFI_FLAGS, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_SMALLICON,
    SHGFI_USEFILEATTRIBUTES, SHGSI_ICON, SHGSI_LARGEICON, SHGetFileInfoW, SHGetStockIconInfo,
    SHSTOCKICONINFO, SIID_RECYCLER, SIID_RECYCLERFULL,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};
use windows::core::PCWSTR;

static SHELL_ICON_LOOKUP: Mutex<()> = Mutex::new(());

/// Decoded 32-bit straight-alpha RGBA icon bitmap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaIcon {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Requested Windows shell icon size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IconSize {
    /// Standard small shell icon (`SHGFI_SMALLICON`, 16x16 at 100% DPI).
    Small,
    /// Standard large shell icon (`SHGFI_LARGEICON`, 32x32 at 100% DPI, scaled on HiDPI).
    Large,
}

impl IconSize {
    fn shgfi_flag(self) -> SHGFI_FLAGS {
        match self {
            Self::Small => SHGFI_SMALLICON,
            Self::Large => SHGFI_LARGEICON,
        }
    }
}

struct IconGuard(HICON);

impl Drop for IconGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: `self.0` was created by `SHGetFileInfoW` (`SHGFI_ICON`) and is destroyed once.
            let _ = unsafe { DestroyIcon(self.0) };
        }
    }
}

pub(crate) struct BitmapGuard(pub(crate) HBITMAP);

impl Drop for BitmapGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: `self.0` is an owned GDI bitmap handle returned by `GetIconInfo` or `GetImage`.
            let _ = unsafe { DeleteObject(HGDIOBJ(self.0.0)) };
        }
    }
}

pub(crate) struct ScreenDcGuard(pub(crate) HDC);

impl Drop for ScreenDcGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: `self.0` was acquired via `GetDC(None)` and must be released via `ReleaseDC`.
            let _ = unsafe { ReleaseDC(None, self.0) };
        }
    }
}

pub(crate) fn read_bitmap_bgra(hdc: HDC, hbm: HBITMAP, width: u32, height: u32) -> Option<Vec<u8>> {
    let pixel_count = (width as usize).checked_mul(height as usize)?;
    let byte_len = pixel_count.checked_mul(4)?;
    let mut buf = vec![0u8; byte_len];

    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: i32::try_from(width).ok()?,
            // Negative height requests a top-down DIB layout.
            biHeight: -i32::try_from(height).ok()?,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: [Default::default()],
    };

    // SAFETY: `hdc` and `hbm` are valid handles, `buf` is sized for `width * height * 4` bytes,
    // and `bmi` describes a 32-bit top-down uncompressed BGRA DIB.
    let lines = unsafe {
        GetDIBits(
            hdc,
            hbm,
            0,
            height,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            &raw mut bmi,
            DIB_RGB_COLORS,
        )
    };

    if lines == 0 { None } else { Some(buf) }
}

fn shgetfileinfo_icon(
    wide_path: &[u16],
    attrs: FILE_FLAGS_AND_ATTRIBUTES,
    flags: SHGFI_FLAGS,
) -> Option<IconGuard> {
    let mut sfi = SHFILEINFOW::default();

    let status = {
        // Concurrent lookups can report success before they populate the icon handle.
        let _lookup = SHELL_ICON_LOOKUP
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // SAFETY: `wide_path` is null-terminated UTF-16 and `sfi` is a valid `SHFILEINFOW` buffer.
        unsafe {
            SHGetFileInfoW(
                PCWSTR(wide_path.as_ptr()),
                attrs,
                Some(&raw mut sfi),
                size_of::<SHFILEINFOW>() as u32,
                flags,
            )
        }
    };

    if status == 0 || sfi.hIcon.is_invalid() {
        None
    } else {
        Some(IconGuard(sfi.hIcon))
    }
}

fn decode_icon_rgba(icon_guard: &IconGuard) -> Option<RgbaIcon> {
    let mut icon_info = ICONINFO::default();
    // SAFETY: `icon_guard.0` is a valid `HICON`. Returned bitmaps are wrapped immediately in RAII guards.
    unsafe { GetIconInfo(icon_guard.0, &raw mut icon_info) }.ok()?;
    let color_guard = BitmapGuard(icon_info.hbmColor);
    let mask_guard = BitmapGuard(icon_info.hbmMask);

    let has_color = !color_guard.0.is_invalid();
    let target_bm = if has_color {
        color_guard.0
    } else if !mask_guard.0.is_invalid() {
        mask_guard.0
    } else {
        return None;
    };

    let mut bm = BITMAP::default();
    // SAFETY: `target_bm` is a valid `HBITMAP` and `bm` is a stack-allocated `BITMAP` struct.
    let got_obj = unsafe {
        GetObjectW(
            HGDIOBJ(target_bm.0),
            size_of::<BITMAP>() as i32,
            Some((&raw mut bm).cast::<c_void>()),
        )
    };
    if got_obj == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
        return None;
    }

    let width = u32::try_from(bm.bmWidth).ok()?;
    let raw_height = u32::try_from(bm.bmHeight).ok()?;
    let height = if has_color {
        raw_height
    } else {
        // Monochrome icons pack AND + XOR masks stacked vertically in hbmMask.
        (raw_height / 2).max(1)
    };

    // SAFETY: `GetDC(None)` acquires the screen DC, immediately wrapped in `ScreenDcGuard`.
    let screen_dc = ScreenDcGuard(unsafe { GetDC(None) });
    if screen_dc.0.is_invalid() {
        return None;
    }

    let mut pixels = read_bitmap_bgra(screen_dc.0, target_bm, width, height)?;

    // Convert BGRA -> RGBA and check whether any pixel has non-zero alpha.
    let mut has_alpha = false;
    for chunk in pixels.chunks_exact_mut(4) {
        chunk.swap(0, 2);
        if chunk[3] != 0 {
            has_alpha = true;
        }
    }

    // Legacy icons without an embedded alpha channel rely on the monochrome AND mask.
    if !has_alpha {
        if !mask_guard.0.is_invalid()
            && let Some(mask_bgra) = read_bitmap_bgra(screen_dc.0, mask_guard.0, width, height)
        {
            for (dst, mask) in pixels.chunks_exact_mut(4).zip(mask_bgra.chunks_exact(4)) {
                // In an icon AND mask, white (non-zero) means transparent and black (0) means opaque.
                dst[3] = if mask[0] == 0 && mask[1] == 0 && mask[2] == 0 {
                    0xFF
                } else {
                    0x00
                };
            }
        } else {
            for chunk in pixels.chunks_exact_mut(4) {
                chunk[3] = 0xFF;
            }
        }
    }

    Some(RgbaIcon {
        width,
        height,
        pixels,
    })
}

/// Extracts the large (32x32 at 100% DPI) Windows shell icon for `path` using
/// `SHGFI_USEFILEATTRIBUTES` (so `path` does not need to exist on disk) and converts
/// it into a top-down 32-bit RGBA image.
#[must_use]
pub fn extract_file_icon_rgba(path: &Path, is_dir: bool) -> Option<RgbaIcon> {
    extract_file_icon_rgba_sized(path, is_dir, IconSize::Large)
}

/// Extracts a Windows shell icon at the requested [`IconSize`] for `path` using
/// `SHGFI_USEFILEATTRIBUTES` (extension/type-based lookup without touching disk).
#[must_use]
pub fn extract_file_icon_rgba_sized(path: &Path, is_dir: bool, size: IconSize) -> Option<RgbaIcon> {
    let _com = ComGuard::init_sta().ok();

    let wide = to_wide_null(path.as_os_str());
    let attrs = if is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    let flags = SHGFI_ICON | size.shgfi_flag() | SHGFI_USEFILEATTRIBUTES;
    let icon_guard = shgetfileinfo_icon(&wide, attrs, flags)?;
    decode_icon_rgba(&icon_guard)
}

/// Resolves the Windows stock icon for the Recycle Bin, empty or full.
///
/// This is the artwork Explorer itself draws, so the sidebar does not have to
/// fake a trash glyph. Call from a blocking worker like the other shell lookups.
#[must_use]
pub fn recycle_bin_icon_rgba(full: bool) -> Option<RgbaIcon> {
    let _com = ComGuard::init_sta().ok();

    let mut info = SHSTOCKICONINFO {
        cbSize: u32::try_from(size_of::<SHSTOCKICONINFO>()).ok()?,
        ..Default::default()
    };
    let id = if full {
        SIID_RECYCLERFULL
    } else {
        SIID_RECYCLER
    };
    // SAFETY: `info` is a `SHSTOCKICONINFO` with `cbSize` set as the API requires, and the
    // returned `HICON` is owned, so it is wrapped in `IconGuard` before anything can leak it.
    let status = unsafe { SHGetStockIconInfo(id, SHGSI_ICON | SHGSI_LARGEICON, &raw mut info) };
    if status.is_err() || info.hIcon.is_invalid() {
        return None;
    }
    decode_icon_rgba(&IconGuard(info.hIcon))
}

/// Resolves the large (32x32 at 100% DPI) Shell icon for an existing item, including known
/// folders and drive roots, without falling back to `SHGFI_USEFILEATTRIBUTES`.
/// Call from a blocking worker because the Shell may access the filesystem or network.
#[must_use]
pub fn extract_existing_icon_rgba(path: &Path) -> Option<RgbaIcon> {
    if path.as_os_str().is_empty() {
        return None;
    }
    let _com = ComGuard::init_sta().ok();
    let wide = to_wide_null(path.as_os_str());
    let icon_guard = shgetfileinfo_icon(
        &wide,
        FILE_FLAGS_AND_ATTRIBUTES(0),
        SHGFI_ICON | IconSize::Large.shgfi_flag(),
    )?;
    decode_icon_rgba(&icon_guard)
}

/// Extracts the large (32x32 at 100% DPI) native Windows shell icon for a real filesystem
/// `path` (such as drives `C:\`, Known Folders like Downloads/Desktop/Documents/Pictures,
/// UserProfile, custom folders, or files).
///
/// Queries `SHGetFileInfoW` directly against the path on disk so Windows returns authentic
/// per-folder and per-drive icons, and falls back safely to `SHGFI_USEFILEATTRIBUTES` if
/// the path does not exist or cannot be queried directly.
#[must_use]
pub fn extract_path_icon_rgba(path: &Path, is_dir: bool) -> Option<RgbaIcon> {
    extract_path_icon_rgba_sized(path, is_dir, IconSize::Large)
}

/// Extracts a native Windows shell icon at the requested [`IconSize`] for a real filesystem
/// `path`, with safe fallback to `SHGFI_USEFILEATTRIBUTES` if the path does not exist.
#[must_use]
pub fn extract_path_icon_rgba_sized(path: &Path, is_dir: bool, size: IconSize) -> Option<RgbaIcon> {
    let _com = ComGuard::init_sta().ok();

    let wide = to_wide_null(path.as_os_str());
    let direct_flags = SHGFI_ICON | size.shgfi_flag();
    let icon_guard = if !path.as_os_str().is_empty() {
        shgetfileinfo_icon(&wide, FILE_FLAGS_AND_ATTRIBUTES(0), direct_flags)
    } else {
        None
    }
    .or_else(|| {
        let attrs = if is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
        shgetfileinfo_icon(&wide, attrs, direct_flags | SHGFI_USEFILEATTRIBUTES)
    })?;

    decode_icon_rgba(&icon_guard)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// The Shell's image lists are process-wide, so concurrent icon requests on
    /// different test threads make each other fail intermittently. The icon tests
    /// that go through `SHGetFileInfo` / `SHGetStockIconInfo` take turns instead.
    static SHELL_ICON_LOCK: Mutex<()> = Mutex::new(());

    fn shell_icon_guard() -> MutexGuard<'static, ()> {
        SHELL_ICON_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn assert_valid_rgba_icon(icon: &RgbaIcon, min_dim: u32) {
        assert!(
            icon.width >= min_dim && icon.height >= min_dim,
            "expected icon at least {min_dim}x{min_dim}, got {}x{}",
            icon.width,
            icon.height
        );
        assert_eq!(icon.width, icon.height);
        assert_eq!(icon.pixels.len(), (icon.width * icon.height * 4) as usize);
        assert!(
            icon.pixels.chunks_exact(4).any(|px| px[3] > 0),
            "icon should contain at least one visible (non-transparent) pixel"
        );
    }

    #[test]
    fn resolves_shell_icon_for_existing_folder() {
        let _shell = shell_icon_guard();
        let folder = tempfile::TempDir::new().unwrap();
        let icon = extract_existing_icon_rgba(folder.path())
            .expect("Windows should resolve the existing folder icon");
        assert_valid_rgba_icon(&icon, 32);
    }

    #[test]
    fn extract_large_icons_for_file_and_directory() {
        let _shell = shell_icon_guard();
        let file_icon = extract_file_icon_rgba(Path::new("example.txt"), false)
            .expect("Windows should provide a large icon for .txt");
        assert_valid_rgba_icon(&file_icon, 32);

        let dir_icon = extract_file_icon_rgba(Path::new("folder"), true)
            .expect("Windows should provide a large icon for directories");
        assert_valid_rgba_icon(&dir_icon, 32);

        let small_icon =
            extract_file_icon_rgba_sized(Path::new("example.txt"), false, IconSize::Small)
                .expect("Windows should provide a small icon for .txt");
        assert_valid_rgba_icon(&small_icon, 16);
        assert!(file_icon.width >= small_icon.width);
    }

    #[test]
    fn stock_recycle_bin_icons_decode_in_both_states() {
        let _shell = shell_icon_guard();
        let empty = recycle_bin_icon_rgba(false)
            .expect("Windows should provide the stock empty Recycle Bin icon");
        assert_valid_rgba_icon(&empty, 32);

        let full = recycle_bin_icon_rgba(true)
            .expect("Windows should provide the stock full Recycle Bin icon");
        assert_valid_rgba_icon(&full, 32);
        assert_eq!(empty.width, full.width);
    }

    #[test]
    fn extract_path_icons_for_real_drive_folder_and_missing_fallback() {
        let _shell = shell_icon_guard();
        let drive_icon = extract_path_icon_rgba(Path::new("C:\\"), true)
            .expect("Windows should provide a large shell icon for C:\\");
        assert_valid_rgba_icon(&drive_icon, 32);

        let temp_dir = std::env::temp_dir();
        let folder_icon = extract_path_icon_rgba(&temp_dir, true)
            .expect("Windows should provide a large shell icon for an existing directory");
        assert_valid_rgba_icon(&folder_icon, 32);

        let missing_fallback = extract_path_icon_rgba(
            Path::new("C:\\__onyx_nonexistent_path_for_icon_test__\\sample.rs"),
            false,
        )
        .expect("Missing path should fall back to SHGFI_USEFILEATTRIBUTES");
        assert_valid_rgba_icon(&missing_fallback, 32);
    }
}
