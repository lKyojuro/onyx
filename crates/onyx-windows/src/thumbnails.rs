//! Windows Shell thumbnail extraction (`IShellItemImageFactory`) into top-down 32-bit RGBA.
//!
//! Shell extraction is what gives Onyx Explorer-parity previews for videos, PDFs, Office
//! documents, vector art, RAW camera files, fonts, and 3D models without linking a decoder
//! for every format. It must run on a blocking worker: `GetImage` pumps Shell handlers on
//! the calling thread's STA.
//!
//! Cloud placeholders (`OneDrive`, `Dropbox`, ...) are served **only** from the Windows
//! thumbnail cache (`SIIGBF_INCACHEONLY`) so a thumbnail request can never hydrate a file
//! and trigger a multi-gigabyte download.

use crate::com::ComGuard;
use crate::icons::{BitmapGuard, ScreenDcGuard, read_bitmap_bgra};
use crate::to_wide_null;
use onyx_core::{OnyxError, OnyxResult, normalize_display_path, to_extended_length_path};
use std::ffi::c_void;
use std::fs;
use std::mem::size_of;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{BITMAP, GetDC, GetObjectW, HBITMAP, HGDIOBJ};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
};
use windows::Win32::System::Com::IBindCtx;
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF, SIIGBF_INCACHEONLY,
    SIIGBF_RESIZETOFIT, SIIGBF_THUMBNAILONLY,
};
use windows::core::PCWSTR;

/// Combined mask of Windows cloud/offline placeholder attributes.
const CLOUD_PLACEHOLDER_ATTRIBUTES_MASK: u32 = FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0
    | FILE_ATTRIBUTE_RECALL_ON_OPEN.0
    | FILE_ATTRIBUTE_OFFLINE.0;

/// Paths at or beyond this length are converted to `\\?\` form so the Shell parser accepts them.
const EXTENDED_PATH_THRESHOLD: usize = 248;

/// Decoded 32-bit straight-alpha RGBA thumbnail produced by a Windows Shell thumbnail handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellThumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Returns `true` when the Win32 attribute flags describe an unhydrated cloud/offline placeholder.
#[must_use]
pub const fn is_cloud_placeholder(file_attributes: u32) -> bool {
    (file_attributes & CLOUD_PLACEHOLDER_ATTRIBUTES_MASK) != 0
}

/// Builds the `SIIGBF` request flags for a file with the given Win32 attributes.
///
/// Placeholders add `SIIGBF_INCACHEONLY` so the Shell answers from its thumbnail cache only
/// instead of recalling the file contents from the cloud provider.
#[must_use]
fn shell_thumbnail_flags(file_attributes: u32) -> SIIGBF {
    let base = SIIGBF_THUMBNAILONLY | SIIGBF_RESIZETOFIT;
    if is_cloud_placeholder(file_attributes) {
        base | SIIGBF_INCACHEONLY
    } else {
        base
    }
}

/// Normalizes `path` for `SHCreateItemFromParsingName`, which parses plain Windows paths
/// cleanly but needs the verbatim `\\?\` prefix once the path can exceed `MAX_PATH`.
#[must_use]
fn normalize_shell_path(path: &Path) -> PathBuf {
    let display = PathBuf::from(normalize_display_path(path));
    if display.as_os_str().len() >= EXTENDED_PATH_THRESHOLD {
        to_extended_length_path(&display)
    } else {
        display
    }
}

fn decode_bitmap_rgba(bitmap: HBITMAP) -> Option<ShellThumbnail> {
    let mut bm = BITMAP::default();
    // SAFETY: `bitmap` is a valid GDI bitmap owned by the caller's `BitmapGuard`, and `bm` is a
    // stack-allocated `BITMAP` output buffer sized by `size_of::<BITMAP>()`.
    let got_obj = unsafe {
        GetObjectW(
            HGDIOBJ(bitmap.0),
            size_of::<BITMAP>() as i32,
            Some((&raw mut bm).cast::<c_void>()),
        )
    };
    if got_obj == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
        return None;
    }

    let width = u32::try_from(bm.bmWidth).ok()?;
    let height = u32::try_from(bm.bmHeight).ok()?;

    // SAFETY: `GetDC(None)` acquires the screen DC, immediately wrapped in `ScreenDcGuard`.
    let screen_dc = ScreenDcGuard(unsafe { GetDC(None) });
    if screen_dc.0.is_invalid() {
        return None;
    }

    let mut rgba = read_bitmap_bgra(screen_dc.0, bitmap, width, height)?;

    // Convert BGRA -> RGBA and detect whether the handler emitted a real alpha channel.
    let mut has_alpha = false;
    for chunk in rgba.chunks_exact_mut(4) {
        chunk.swap(0, 2);
        if chunk[3] != 0 {
            has_alpha = true;
        }
    }

    // Several third-party Shell thumbnail handlers return a fully transparent alpha channel
    // for opaque artwork; force those to opaque so the list view does not render nothing.
    if !has_alpha {
        for chunk in rgba.chunks_exact_mut(4) {
            chunk[3] = 0xFF;
        }
    }

    Some(ShellThumbnail {
        width,
        height,
        rgba,
    })
}

/// Extracts a Windows Shell thumbnail for `path` scaled to fit `target_px x target_px`.
///
/// Returns `Ok(None)` when the Shell has no thumbnail to offer (no registered handler, the
/// extraction is still pending, the file is a cloud placeholder that is not cached yet, or
/// the path is not a regular file). Callers should treat `Ok(None)` as "draw a fallback icon".
///
/// Call from a blocking worker: `GetImage` runs third-party Shell handlers on this thread.
pub fn extract_shell_thumbnail_rgba(
    path: &Path,
    target_px: u32,
) -> OnyxResult<Option<ShellThumbnail>> {
    if target_px == 0 {
        return Err(OnyxError::Preview {
            message: "Shell thumbnail target_px must be greater than zero".to_string(),
        });
    }

    let file_attributes = match fs::symlink_metadata(path) {
        Ok(meta) => meta.file_attributes(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // An unreadable entry has nothing to thumbnail; fall back to the type icon.
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return Ok(None),
        Err(err) => return Err(OnyxError::Io(err)),
    };

    let flags = shell_thumbnail_flags(file_attributes);
    let _com = ComGuard::init_sta()?;

    let normalized = normalize_shell_path(path);
    let wide = to_wide_null(normalized.as_os_str());

    // SAFETY: `wide` is a null-terminated UTF-16 Windows path and `None` passes no bind context.
    // The returned factory owns its own reference and is released on drop.
    let factory: IShellItemImageFactory = match unsafe {
        SHCreateItemFromParsingName::<PCWSTR, Option<&IBindCtx>, IShellItemImageFactory>(
            PCWSTR(wide.as_ptr()),
            None,
        )
    } {
        Ok(factory) => factory,
        Err(_) => return Ok(None),
    };

    let side = i32::try_from(target_px).map_err(|_| OnyxError::Preview {
        message: format!("Shell thumbnail size {target_px} px is out of range"),
    })?;

    // SAFETY: `factory` is a live Shell item and `SIZE` is a plain `i32` pair. Failures
    // (`WTS_E_EXTRACTIONPENDING`, `E_FAIL`, missing handler, cache miss) mean "no thumbnail".
    let bitmap = match unsafe { factory.GetImage(SIZE { cx: side, cy: side }, flags) } {
        Ok(bitmap) => bitmap,
        Err(_) => return Ok(None),
    };
    let bitmap_guard = BitmapGuard(bitmap);

    Ok(decode_bitmap_rgba(bitmap_guard.0))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_PINNED,
    };

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut chunk = Vec::with_capacity(data.len() + 12);
        chunk.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        let mut crc_input = Vec::from(*kind);
        crc_input.extend_from_slice(data);
        chunk.extend_from_slice(kind);
        chunk.extend_from_slice(data);
        chunk.extend_from_slice(&crc32(&crc_input).to_be_bytes());
        chunk
    }

    /// Writes a 1x1 opaque red PNG without pulling an encoder into this crate's dev-dependencies.
    fn write_red_pixel_png(path: &Path) {
        let ihdr: [u8; 13] = [0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0];
        // zlib header + one stored deflate block containing `[filter=0, R, G, B]` + Adler-32.
        let idat: [u8; 15] = [
            0x78, 0x01, 0x01, 0x04, 0x00, 0xFB, 0xFF, 0x00, 0xFF, 0x00, 0x00, 0x03, 0x01, 0x01,
            0x00,
        ];

        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
        png.extend_from_slice(&png_chunk(b"IDAT", &idat));
        png.extend_from_slice(&png_chunk(b"IEND", &[]));
        std::fs::write(path, png).unwrap();
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("does_not_exist.mp4");
        assert!(extract_shell_thumbnail_rgba(&path, 96).unwrap().is_none());
    }

    #[test]
    fn zero_target_pixels_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("anything.png");
        std::fs::write(&path, b"not really a png").unwrap();

        let err = extract_shell_thumbnail_rgba(&path, 0).unwrap_err();
        match err {
            OnyxError::Preview { message } => assert!(message.contains("target_px")),
            other => panic!("expected OnyxError::Preview, got {other:?}"),
        }
    }

    #[test]
    fn cache_only_flag_is_selected_for_cloud_placeholder_attributes() {
        assert!(is_cloud_placeholder(FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0));
        assert!(is_cloud_placeholder(FILE_ATTRIBUTE_RECALL_ON_OPEN.0));
        assert!(is_cloud_placeholder(FILE_ATTRIBUTE_OFFLINE.0));
        assert!(is_cloud_placeholder(
            FILE_ATTRIBUTE_ARCHIVE.0 | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0
        ));

        assert!(!is_cloud_placeholder(FILE_ATTRIBUTE_NORMAL.0));
        assert!(!is_cloud_placeholder(FILE_ATTRIBUTE_ARCHIVE.0));
        assert!(!is_cloud_placeholder(FILE_ATTRIBUTE_PINNED.0));
        assert!(!is_cloud_placeholder(0));

        let placeholder_flags = shell_thumbnail_flags(FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0);
        assert!(placeholder_flags.contains(SIIGBF_INCACHEONLY));
        assert!(placeholder_flags.contains(SIIGBF_THUMBNAILONLY));

        let normal_flags = shell_thumbnail_flags(FILE_ATTRIBUTE_ARCHIVE.0);
        assert!(!normal_flags.contains(SIIGBF_INCACHEONLY));
        assert!(normal_flags.contains(SIIGBF_THUMBNAILONLY));
        assert!(normal_flags.contains(SIIGBF_RESIZETOFIT));
    }

    #[test]
    fn real_files_execute_safely_for_png_svg_and_txt() {
        let dir = tempfile::TempDir::new().unwrap();

        let png_path = dir.path().join("sample.png");
        write_red_pixel_png(&png_path);

        let svg_path = dir.path().join("shape.svg");
        std::fs::write(
            &svg_path,
            br##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><rect width="64" height="64" fill="#3b82f6"/></svg>"##,
        )
        .unwrap();

        let txt_path = dir.path().join("notes.txt");
        std::fs::write(&txt_path, b"plain text has no thumbnail handler").unwrap();

        for path in [&png_path, &svg_path, &txt_path] {
            match extract_shell_thumbnail_rgba(path, 96) {
                Ok(Some(thumb)) => assert_eq!(
                    thumb.rgba.len(),
                    (thumb.width * thumb.height * 4) as usize,
                    "thumbnail buffer must cover every pixel of {}",
                    path.display()
                ),
                Ok(None) => {}
                Err(err) => panic!("unexpected error for {}: {err:?}", path.display()),
            }
        }
    }
}
