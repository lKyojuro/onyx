//! Native Windows shell file icon extraction into top-down 32-bit RGBA pixel buffers.

use crate::com::ComGuard;
use crate::to_wide_null;
use std::ffi::c_void;
use std::mem::size_of;
use std::path::Path;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
    GetObjectW, HBITMAP, HDC, HGDIOBJ, ReleaseDC,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::UI::Shell::{
    SHFILEINFOW, SHGFI_ICON, SHGFI_SMALLICON, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};
use windows::core::PCWSTR;

/// Decoded 32-bit straight-alpha RGBA icon bitmap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaIcon {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
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

struct BitmapGuard(HBITMAP);

impl Drop for BitmapGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: `self.0` is an owned GDI bitmap handle returned by `GetIconInfo`.
            let _ = unsafe { DeleteObject(HGDIOBJ(self.0.0)) };
        }
    }
}

struct ScreenDcGuard(HDC);

impl Drop for ScreenDcGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: `self.0` was acquired via `GetDC(None)` and must be released via `ReleaseDC`.
            let _ = unsafe { ReleaseDC(None, self.0) };
        }
    }
}

fn read_bitmap_bgra(hdc: HDC, hbm: HBITMAP, width: u32, height: u32) -> Option<Vec<u8>> {
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

/// Extracts the small Windows shell icon for `path` (using `SHGFI_USEFILEATTRIBUTES` so the
/// path does not need to exist on disk) and converts it into a top-down 32-bit RGBA image.
#[must_use]
pub fn extract_file_icon_rgba(path: &Path, is_dir: bool) -> Option<RgbaIcon> {
    let _com = ComGuard::init_sta().ok();

    let wide = to_wide_null(path.as_os_str());
    let attrs = if is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    let flags = SHGFI_ICON | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES;
    let mut sfi = SHFILEINFOW::default();

    // SAFETY: `wide` is null-terminated UTF-16 and `sfi` is a valid `SHFILEINFOW` buffer.
    let status = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            attrs,
            Some(&raw mut sfi),
            size_of::<SHFILEINFOW>() as u32,
            flags,
        )
    };

    if status == 0 || sfi.hIcon.is_invalid() {
        return None;
    }
    let icon_guard = IconGuard(sfi.hIcon);

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn extract_icons_for_file_and_directory() {
        let file_icon = extract_file_icon_rgba(Path::new("example.txt"), false)
            .expect("Windows should provide a small icon for .txt");
        assert!(file_icon.width > 0);
        assert!(file_icon.height > 0);
        assert_eq!(
            file_icon.pixels.len(),
            (file_icon.width * file_icon.height * 4) as usize
        );

        let dir_icon = extract_file_icon_rgba(Path::new("folder"), true)
            .expect("Windows should provide a small icon for directories");
        assert!(dir_icon.width > 0);
        assert!(dir_icon.height > 0);
        assert_eq!(
            dir_icon.pixels.len(),
            (dir_icon.width * dir_icon.height * 4) as usize
        );
    }
}
