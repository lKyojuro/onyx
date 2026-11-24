use crate::{PreviewContent, PreviewLimits};
use image::{GenericImageView, ImageFormat, ImageReader, Limits};
use onyx_core::{OnyxError, OnyxResult, format_bytes};
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;
use tokio_util::sync::CancellationToken;

/// Maximum untrusted image dimension accepted by the decoder before rejecting decompression bombs.
const MAX_DECODE_DIMENSION: u32 = 16_384;
/// Maximum memory allocation allowed during image decoding (128 MiB).
const MAX_DECODE_ALLOC_BYTES: u64 = 128 * 1024 * 1024;

/// Returns whether the file extension corresponds to a raster image supported by the `image` crate.
#[must_use]
pub(crate) fn is_supported_image_extension(path: &Path) -> bool {
    let Some(ext) = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
    else {
        return false;
    };
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tif" | "tiff"
    )
}

/// Checks magic bytes for a raster image format supported by the `image` crate.
#[must_use]
pub(crate) fn is_supported_image_magic(header: &[u8]) -> bool {
    image::guess_format(header).is_ok_and(|fmt| {
        matches!(
            fmt,
            ImageFormat::Png
                | ImageFormat::Jpeg
                | ImageFormat::Gif
                | ImageFormat::WebP
                | ImageFormat::Bmp
                | ImageFormat::Ico
                | ImageFormat::Tiff
        )
    })
}

/// Safely loads, decodes, and optionally thumbnails an untrusted raster image within configured limits.
pub(crate) fn load_image_preview(
    path: &Path,
    file_size: u64,
    limits: &PreviewLimits,
    cancel: &CancellationToken,
) -> OnyxResult<PreviewContent> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    if file_size > limits.max_image_bytes {
        return Ok(PreviewContent::Unavailable {
            reason: format!(
                "Image size ({}) exceeds preview limit ({})",
                format_bytes(file_size),
                format_bytes(limits.max_image_bytes)
            ),
        });
    }

    let file = File::open(path)?;
    let read_cap = limits.max_image_bytes.saturating_add(1);
    let mut bytes =
        Vec::with_capacity(usize::try_from(file_size.min(4 * 1024 * 1024)).unwrap_or(0));
    file.take(read_cap).read_to_end(&mut bytes)?;

    if (bytes.len() as u64) > limits.max_image_bytes {
        return Ok(PreviewContent::Unavailable {
            reason: format!(
                "Image exceeds preview limit ({})",
                format_bytes(limits.max_image_bytes)
            ),
        });
    }

    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    let mut reader = match ImageReader::new(Cursor::new(&bytes)).with_guessed_format() {
        Ok(r) => r,
        Err(err) => {
            return Ok(PreviewContent::Unavailable {
                reason: format!("Could not inspect image header: {err}"),
            });
        }
    };

    let format_name = reader
        .format()
        .map(format_label)
        .unwrap_or("Unknown")
        .to_string();

    let mut decode_limits = Limits::default();
    decode_limits.max_image_width = Some(MAX_DECODE_DIMENSION);
    decode_limits.max_image_height = Some(MAX_DECODE_DIMENSION);
    decode_limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    reader.limits(decode_limits);

    let decoded = match reader.decode() {
        Ok(img) => img,
        Err(err) => {
            return Ok(PreviewContent::Unavailable {
                reason: format!("Unable to decode image safely: {err}"),
            });
        }
    };

    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    let (original_width, original_height) = decoded.dimensions();
    let max_dim = limits.max_image_dim.max(1);
    let final_img = if original_width > max_dim || original_height > max_dim {
        decoded.thumbnail(max_dim, max_dim)
    } else {
        decoded
    };

    let rgba_img = final_img.to_rgba8();
    let width = rgba_img.width();
    let height = rgba_img.height();
    let rgba = rgba_img.into_raw();

    Ok(PreviewContent::Image {
        width,
        height,
        original_width,
        original_height,
        rgba,
        format: format_name,
    })
}

fn format_label(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "PNG",
        ImageFormat::Jpeg => "JPEG",
        ImageFormat::Gif => "GIF",
        ImageFormat::WebP => "WEBP",
        ImageFormat::Bmp => "BMP",
        ImageFormat::Ico => "ICO",
        ImageFormat::Tiff => "TIFF",
        _ => "Image",
    }
}
