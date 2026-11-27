//! Async, concurrency-bounded, byte-budgeted LRU thumbnail loader and cache.

use image::{GenericImageView, ImageFormat, ImageReader, Limits};
use lru::LruCache;
use onyx_core::{
    OnyxError, OnyxResult, format_bytes, normalize_display_path, to_extended_length_path,
};
use std::collections::HashMap;
use std::fs::{File, Metadata};
use std::io::{Cursor, Read};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

/// Maximum untrusted image dimension accepted before rejecting decompression bombs.
const MAX_DECODE_DIMENSION: u32 = 16_384;
/// Maximum memory allocation allowed during a single thumbnail decode (128 MiB).
const MAX_DECODE_ALLOC_BYTES: u64 = 128 * 1024 * 1024;

/// Windows file attribute bit set when file data is not present locally and reading will recall from cloud storage.
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
/// Windows file attribute bit set when opening the file will recall it from remote/cloud storage.
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
/// Windows file attribute bit set when file data is offline / moved to remote storage.
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
/// Combined mask of Windows cloud/offline placeholder attributes that must never be hydrated by thumbnailing.
const CLOUD_PLACEHOLDER_ATTRIBUTES_MASK: u32 =
    FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_OFFLINE;

/// Returns `true` if the given Win32 file attribute flags indicate an unhydrated cloud/offline placeholder.
#[must_use]
pub const fn is_cloud_placeholder_attributes(file_attributes: u32) -> bool {
    (file_attributes & CLOUD_PLACEHOLDER_ATTRIBUTES_MASK) != 0
}

/// Normalizes a filesystem path for thumbnail cache lookup so extended-length (`\\?\`),
/// forward/backward slash variants, and Windows case differences map to a single canonical key.
#[must_use]
pub fn normalize_thumbnail_key_path(path: &Path) -> PathBuf {
    let display = normalize_display_path(path).replace('/', "\\");
    let trimmed = if display.len() > 3 {
        display.trim_end_matches('\\')
    } else {
        display.as_str()
    };
    #[cfg(windows)]
    {
        PathBuf::from(trimmed.to_lowercase())
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(trimmed)
    }
}

/// Cache lookup key combining normalized path, modification timestamp (seconds + subsecond nanos),
/// file size, and target size.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ThumbnailKey {
    pub path: PathBuf,
    pub modified_secs: i64,
    pub modified_nanos: u32,
    pub size: u64,
    pub target_px: u32,
}

impl ThumbnailKey {
    /// Creates a [`ThumbnailKey`] with a normalized path key.
    #[must_use]
    pub fn new(
        path: &Path,
        modified_secs: i64,
        modified_nanos: u32,
        size: u64,
        target_px: u32,
    ) -> Self {
        Self {
            path: normalize_thumbnail_key_path(path),
            modified_secs,
            modified_nanos,
            size,
            target_px,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CoarseLookupKey {
    path: PathBuf,
    modified_secs: i64,
    size: u64,
    target_px: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileSignature {
    modified_secs: i64,
    modified_nanos: u32,
    size: u64,
    /// `true` when the file is an unhydrated cloud/offline placeholder.
    is_cloud_placeholder: bool,
}

/// Decoded RGBA8 thumbnail buffer with pixel dimensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Configuration limits for [`ThumbnailEngine`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailConfig {
    /// Maximum number of thumbnails retained in the LRU cache (default: 256).
    pub max_cache_entries: usize,
    /// Maximum total RGBA bytes retained across all cached thumbnails (default: 32 MiB).
    pub max_cache_bytes: usize,
    /// Maximum source image file size in bytes eligible for thumbnailing (default: 25 MiB).
    pub max_source_file_bytes: u64,
    /// Maximum concurrent blocking decode workers (default: 4).
    pub max_concurrent_decodes: usize,
}

impl Default for ThumbnailConfig {
    fn default() -> Self {
        Self {
            max_cache_entries: 256,
            max_cache_bytes: 32 * 1024 * 1024,
            max_source_file_bytes: 25 * 1024 * 1024,
            max_concurrent_decodes: 4,
        }
    }
}

/// Returns `true` if `path` has an extension this crate can decode in-process with the `image` crate.
#[must_use]
pub fn is_raster_thumbnail_supported(path: &Path) -> bool {
    let Some(ext) = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
    else {
        return false;
    };
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "ico" | "tif" | "tiff"
    )
}

/// Returns `true` if `path` has a non-raster extension that Windows Shell thumbnail handlers
/// (or third-party ones) can serve: videos, documents, vector/RAW images, fonts, and 3D models.
#[must_use]
pub fn is_shell_thumbnail_supported(path: &Path) -> bool {
    let Some(ext) = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
    else {
        return false;
    };
    matches!(
        ext.as_str(),
        // Videos
        "mp4"
            | "mkv"
            | "avi"
            | "mov"
            | "wmv"
            | "webm"
            | "m4v"
            | "mpg"
            | "mpeg"
            | "ts"
            | "mts"
            | "flv"
            | "3gp"
            // Documents
            | "pdf"
            | "docx"
            | "doc"
            | "xlsx"
            | "xls"
            | "pptx"
            | "ppt"
            | "odt"
            | "ods"
            | "odp"
            | "rtf"
            | "epub"
            | "xps"
            // Vector / RAW / modern images
            | "svg"
            | "heic"
            | "heif"
            | "avif"
            | "jxl"
            | "psd"
            | "ai"
            | "eps"
            | "raw"
            | "cr2"
            | "cr3"
            | "nef"
            | "arw"
            | "dng"
            | "orf"
            | "rw2"
            | "raf"
            | "pef"
            | "srw"
            // Fonts / 3D
            | "ttf"
            | "otf"
            | "glb"
            | "gltf"
            | "stl"
            | "obj"
            | "3mf"
    )
}

/// Returns `true` if `path` is eligible for thumbnail generation, either in-process (raster) or
/// through the Windows Shell.
#[must_use]
pub fn is_thumbnail_supported(path: &Path) -> bool {
    is_raster_thumbnail_supported(path) || is_shell_thumbnail_supported(path)
}

#[derive(Debug)]
struct CacheState {
    lru: LruCache<ThumbnailKey, ThumbnailImage>,
    latest_nanos: HashMap<CoarseLookupKey, u32>,
    current_bytes: usize,
    max_entries: usize,
    max_bytes: usize,
}

impl CacheState {
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        let cap = NonZeroUsize::new(max_entries.max(1)).unwrap_or(NonZeroUsize::MIN);
        Self {
            lru: LruCache::new(cap),
            latest_nanos: HashMap::new(),
            current_bytes: 0,
            max_entries,
            max_bytes,
        }
    }

    fn get(&mut self, key: &ThumbnailKey) -> Option<ThumbnailImage> {
        self.lru.get(key).cloned()
    }

    fn get_by_secs(
        &mut self,
        path: &Path,
        modified_secs: i64,
        size: u64,
        target_px: u32,
    ) -> Option<ThumbnailImage> {
        let norm_path = normalize_thumbnail_key_path(path);
        let coarse = CoarseLookupKey {
            path: norm_path.clone(),
            modified_secs,
            size,
            target_px,
        };
        let modified_nanos = self.latest_nanos.get(&coarse).copied().unwrap_or(0);
        let key = ThumbnailKey {
            path: norm_path,
            modified_secs,
            modified_nanos,
            size,
            target_px,
        };
        self.lru.get(&key).cloned()
    }

    fn insert(&mut self, key: ThumbnailKey, image: ThumbnailImage) {
        let entry_bytes = image.rgba.len();
        if self.max_entries == 0 || entry_bytes > self.max_bytes {
            return;
        }

        let coarse = CoarseLookupKey {
            path: key.path.clone(),
            modified_secs: key.modified_secs,
            size: key.size,
            target_px: key.target_px,
        };

        // If a previous entry existed for the same (path, modified_secs, size, target_px) with
        // different sub-second nanoseconds (same-second rewrite), evict the stale entry immediately.
        if let Some(prev_nanos) = self.latest_nanos.insert(coarse, key.modified_nanos) {
            let prev_key = ThumbnailKey {
                path: key.path.clone(),
                modified_secs: key.modified_secs,
                modified_nanos: prev_nanos,
                size: key.size,
                target_px: key.target_px,
            };
            if let Some(old) = self.lru.pop(&prev_key) {
                self.current_bytes = self.current_bytes.saturating_sub(old.rgba.len());
            }
        }

        if let Some(old) = self.lru.pop(&key) {
            self.current_bytes = self.current_bytes.saturating_sub(old.rgba.len());
        }

        while self.lru.len() >= self.max_entries
            || self.current_bytes.saturating_add(entry_bytes) > self.max_bytes
        {
            let Some((evicted_key, evicted)) = self.lru.pop_lru() else {
                break;
            };
            self.current_bytes = self.current_bytes.saturating_sub(evicted.rgba.len());
            let evicted_coarse = CoarseLookupKey {
                path: evicted_key.path,
                modified_secs: evicted_key.modified_secs,
                size: evicted_key.size,
                target_px: evicted_key.target_px,
            };
            if self.latest_nanos.get(&evicted_coarse).copied() == Some(evicted_key.modified_nanos) {
                self.latest_nanos.remove(&evicted_coarse);
            }
        }

        self.current_bytes = self.current_bytes.saturating_add(entry_bytes);
        self.lru.put(key, image);
    }

    fn clear(&mut self) {
        self.lru.clear();
        self.latest_nanos.clear();
        self.current_bytes = 0;
    }

    fn stats(&self) -> (usize, usize) {
        (self.lru.len(), self.current_bytes)
    }
}

/// Clone-friendly async thumbnail loader and byte-bounded LRU cache.
#[derive(Debug, Clone)]
pub struct ThumbnailEngine {
    config: ThumbnailConfig,
    cache: Arc<Mutex<CacheState>>,
    semaphore: Arc<Semaphore>,
}

impl Default for ThumbnailEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ThumbnailEngine {
    /// Creates a new [`ThumbnailEngine`] with default [`ThumbnailConfig`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(ThumbnailConfig::default())
    }

    /// Creates a new [`ThumbnailEngine`] with custom [`ThumbnailConfig`].
    #[must_use]
    pub fn with_config(config: ThumbnailConfig) -> Self {
        let concurrency = config.max_concurrent_decodes.max(1);
        let cache = CacheState::new(config.max_cache_entries, config.max_cache_bytes);
        Self {
            config,
            cache: Arc::new(Mutex::new(cache)),
            semaphore: Arc::new(Semaphore::new(concurrency)),
        }
    }

    /// Returns the active [`ThumbnailConfig`].
    #[must_use]
    pub fn config(&self) -> &ThumbnailConfig {
        &self.config
    }

    /// Performs a fast, non-blocking cache lookup for a thumbnail with known file metadata.
    #[must_use]
    pub fn get_cached(
        &self,
        path: &Path,
        modified_secs: i64,
        size: u64,
        target_px: u32,
    ) -> Option<ThumbnailImage> {
        let mut guard = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.get_by_secs(path, modified_secs, size, target_px)
    }

    fn get_cached_key(&self, key: &ThumbnailKey) -> Option<ThumbnailImage> {
        let mut guard = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.get(key)
    }

    /// Clears all entries from the thumbnail cache and resets byte accounting.
    pub fn clear_cache(&self) {
        let mut guard = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.clear();
    }

    /// Returns `(entry_count, total_rgba_bytes)` currently stored in the cache.
    #[must_use]
    pub fn cache_stats(&self) -> (usize, usize) {
        let guard = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.stats()
    }

    /// Loads or generates an RGBA8 thumbnail scaled to fit within `target_px x target_px`.
    ///
    /// Checks the in-memory LRU cache first using the file's modification timestamp and byte size.
    /// On a cache miss, acquires a bounded concurrency permit and decodes the image on Tokio's
    /// blocking pool while respecting `cancel`.
    pub async fn load_thumbnail(
        &self,
        path: PathBuf,
        target_px: u32,
        cancel: CancellationToken,
    ) -> OnyxResult<ThumbnailImage> {
        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        if target_px == 0 {
            return Err(OnyxError::Preview {
                message: "Thumbnail target_px must be greater than zero".to_string(),
            });
        }

        let is_raster = is_raster_thumbnail_supported(&path);
        if !is_raster && !is_shell_thumbnail_supported(&path) {
            return Err(OnyxError::Preview {
                message: format!("Unsupported thumbnail extension for '{}'", path.display()),
            });
        }

        let ext_path = to_extended_length_path(&path);
        let sig = read_file_signature(&path, &ext_path).await?;

        // The size budget and the placeholder guard protect the in-process `image` decoder only.
        // Videos and documents routed through the Shell are routinely larger than 25 MiB, and the
        // Shell path never hydrates placeholders (`SIIGBF_INCACHEONLY`).
        if is_raster {
            if sig.is_cloud_placeholder {
                return Err(OnyxError::Preview {
                    message: format!(
                        "Skipping unhydrated cloud placeholder file '{}'",
                        path.display()
                    ),
                });
            }

            if sig.size > self.config.max_source_file_bytes {
                return Err(OnyxError::Preview {
                    message: format!(
                        "Image size ({}) exceeds thumbnail limit ({})",
                        format_bytes(sig.size),
                        format_bytes(self.config.max_source_file_bytes)
                    ),
                });
            }
        }

        let key = ThumbnailKey::new(
            &path,
            sig.modified_secs,
            sig.modified_nanos,
            sig.size,
            target_px,
        );

        if let Some(cached) = self.get_cached_key(&key) {
            return Ok(cached);
        }

        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        let permit = self.acquire_permit(&cancel).await?;

        if cancel.is_cancelled() {
            drop(permit);
            return Err(OnyxError::Cancelled);
        }

        // Re-check cache after acquiring semaphore permit in case a concurrent task populated it.
        if let Some(cached) = self.get_cached_key(&key) {
            drop(permit);
            return Ok(cached);
        }

        let thumbnail = if is_raster {
            match self
                .decode_raster_thumbnail(permit, ext_path, sig.size, target_px, cancel.clone())
                .await
            {
                Ok(image) => image,
                Err(err) => {
                    // Formats that the `image` crate cannot decode may still have a Shell handler.
                    // Placeholders and oversized files were rejected above, so this stays cheap.
                    let fallback = if matches!(err, OnyxError::Cancelled) {
                        Ok(None)
                    } else {
                        self.shell_thumbnail(&path, target_px, &cancel).await
                    };
                    match fallback {
                        Ok(Some(image)) => image,
                        Ok(None) | Err(_) => return Err(err),
                    }
                }
            }
        } else {
            drop(permit);
            match self.shell_thumbnail(&path, target_px, &cancel).await? {
                Some(image) => image,
                None => {
                    return Err(OnyxError::Preview {
                        message: format!(
                            "Windows Shell produced no thumbnail for '{}'",
                            path.display()
                        ),
                    });
                }
            }
        };

        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        {
            let mut guard = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.insert(key, thumbnail.clone());
        }

        Ok(thumbnail)
    }

    async fn acquire_permit(&self, cancel: &CancellationToken) -> OnyxResult<OwnedSemaphorePermit> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(OnyxError::Cancelled),
            res = Arc::clone(&self.semaphore).acquire_owned() => {
                res.map_err(|_| OnyxError::Preview {
                    message: "Thumbnail concurrency semaphore closed".to_string(),
                })
            }
        }
    }

    /// Decodes a raster image on the blocking pool while holding `permit` for the decode's lifetime.
    async fn decode_raster_thumbnail(
        &self,
        permit: OwnedSemaphorePermit,
        ext_path: PathBuf,
        file_size: u64,
        target_px: u32,
        cancel: CancellationToken,
    ) -> OnyxResult<ThumbnailImage> {
        let max_source_bytes = self.config.max_source_file_bytes;
        let worker_cancel = cancel.clone();

        let join_result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            decode_thumbnail_blocking(
                &ext_path,
                file_size,
                max_source_bytes,
                target_px,
                &worker_cancel,
            )
        })
        .await;

        match join_result {
            Ok(result) => result,
            Err(join_err) => Err(OnyxError::Preview {
                message: format!("Thumbnail worker task failed: {join_err}"),
            }),
        }
    }

    /// Extracts a thumbnail through the Windows Shell on the blocking pool, bounded by `permit`.
    async fn shell_thumbnail(
        &self,
        path: &Path,
        target_px: u32,
        cancel: &CancellationToken,
    ) -> OnyxResult<Option<ThumbnailImage>> {
        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        let permit = self.acquire_permit(cancel).await?;
        let owned_path = path.to_path_buf();

        let join_result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            extract_shell_thumbnail_image(&owned_path, target_px)
        })
        .await;

        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        match join_result {
            Ok(result) => result,
            Err(join_err) => Err(OnyxError::Preview {
                message: format!("Shell thumbnail worker task failed: {join_err}"),
            }),
        }
    }
}

async fn read_file_signature(original_path: &Path, ext_path: &Path) -> OnyxResult<FileSignature> {
    let meta = match tokio::fs::metadata(ext_path).await {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(OnyxError::NotFound {
                path: original_path.to_path_buf(),
            });
        }
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(OnyxError::PermissionDenied {
                path: original_path.to_path_buf(),
            });
        }
        Err(err) => return Err(OnyxError::Io(err)),
    };

    parse_file_signature(original_path, &meta)
}

fn parse_file_signature(original_path: &Path, meta: &Metadata) -> OnyxResult<FileSignature> {
    // Cloud placeholder detection is recorded, not enforced: Shell-routed formats can still be
    // thumbnailed from the Windows thumbnail cache (`SIIGBF_INCACHEONLY`). Only the in-process
    // `image` decoder must refuse to hydrate a placeholder.
    #[cfg(windows)]
    let is_cloud_placeholder = {
        use std::os::windows::fs::MetadataExt;
        is_cloud_placeholder_attributes(meta.file_attributes())
    };
    #[cfg(not(windows))]
    let is_cloud_placeholder = false;

    if !meta.is_file() {
        return Err(OnyxError::Preview {
            message: format!("'{}' is not a regular file", original_path.display()),
        });
    }

    let (modified_secs, modified_nanos) = meta
        .modified()
        .ok()
        .and_then(|mtime| match mtime.duration_since(UNIX_EPOCH) {
            Ok(dur) => i64::try_from(dur.as_secs())
                .ok()
                .map(|s| (s, dur.subsec_nanos())),
            Err(err) => {
                let dur = err.duration();
                i64::try_from(dur.as_secs())
                    .ok()
                    .map(|s| (-s, dur.subsec_nanos()))
            }
        })
        .unwrap_or((0, 0));

    Ok(FileSignature {
        modified_secs,
        modified_nanos,
        size: meta.len(),
        is_cloud_placeholder,
    })
}

/// Extracts a thumbnail through the Windows Shell on the calling blocking worker.
///
/// Returns `Ok(None)` when the Shell has no thumbnail handler result for `path`.
#[cfg(windows)]
fn extract_shell_thumbnail_image(
    path: &Path,
    target_px: u32,
) -> OnyxResult<Option<ThumbnailImage>> {
    onyx_windows::extract_shell_thumbnail_rgba(path, target_px).map(|decoded| {
        decoded.map(|thumbnail| ThumbnailImage {
            width: thumbnail.width,
            height: thumbnail.height,
            rgba: thumbnail.rgba,
        })
    })
}

/// Shell thumbnail extraction is Windows-only; other targets have no handler to ask.
#[cfg(not(windows))]
fn extract_shell_thumbnail_image(
    _path: &Path,
    _target_px: u32,
) -> OnyxResult<Option<ThumbnailImage>> {
    Ok(None)
}

fn decode_thumbnail_blocking(
    path: &Path,
    file_size: u64,
    max_source_bytes: u64,
    target_px: u32,
    cancel: &CancellationToken,
) -> OnyxResult<ThumbnailImage> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    let file = File::open(path)?;
    let read_cap = max_source_bytes.saturating_add(1);
    let initial_cap = usize::try_from(file_size.min(4 * 1024 * 1024)).unwrap_or(0);
    let mut bytes = Vec::with_capacity(initial_cap);
    file.take(read_cap).read_to_end(&mut bytes)?;

    if (bytes.len() as u64) > max_source_bytes {
        return Err(OnyxError::Preview {
            message: format!(
                "Image exceeds thumbnail limit ({})",
                format_bytes(max_source_bytes)
            ),
        });
    }

    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    let mut reader = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|err| OnyxError::Preview {
            message: format!("Failed to inspect image format: {err}"),
        })?;

    if !reader.format().is_some_and(is_supported_format) {
        return Err(OnyxError::Preview {
            message: "Unsupported or unrecognized raster image format".to_string(),
        });
    }

    let mut decode_limits = Limits::default();
    decode_limits.max_image_width = Some(MAX_DECODE_DIMENSION);
    decode_limits.max_image_height = Some(MAX_DECODE_DIMENSION);
    decode_limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    reader.limits(decode_limits);

    let decoded = reader.decode().map_err(|err| OnyxError::Preview {
        message: format!("Failed to decode image for thumbnail: {err}"),
    })?;

    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    let (orig_w, orig_h) = decoded.dimensions();
    let max_dim = target_px.max(1);
    let scaled = if orig_w > max_dim || orig_h > max_dim {
        decoded.thumbnail(max_dim, max_dim)
    } else {
        decoded
    };

    let rgba_img = scaled.to_rgba8();
    let width = rgba_img.width();
    let height = rgba_img.height();
    let rgba = rgba_img.into_raw();

    Ok(ThumbnailImage {
        width,
        height,
        rgba,
    })
}

fn is_supported_format(format: ImageFormat) -> bool {
    matches!(
        format,
        ImageFormat::Png
            | ImageFormat::Jpeg
            | ImageFormat::Gif
            | ImageFormat::WebP
            | ImageFormat::Bmp
            | ImageFormat::Ico
            | ImageFormat::Tiff
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb, Rgba};
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn supported_extensions_are_detected() {
        for ext in [
            "a.png", "a.PNG", "a.jpg", "a.jpeg", "a.webp", "a.gif", "a.bmp", "a.ico", "a.tiff",
        ] {
            assert!(
                is_thumbnail_supported(Path::new(ext)),
                "expected {ext} to be supported"
            );
        }
        for ext in ["a.pdf", "a.mp4", "a.svg", "a.cr2"] {
            assert!(
                is_thumbnail_supported(Path::new(ext)),
                "expected Shell-thumbnailable {ext} to be supported"
            );
        }
        for unsupported in ["a.txt", "a.rs", "no_ext"] {
            assert!(
                !is_thumbnail_supported(Path::new(unsupported)),
                "expected {unsupported} to be rejected"
            );
        }
    }

    #[test]
    fn raster_and_shell_predicates_are_disjoint() {
        for ext in [
            "a.png", "a.jpg", "a.jpeg", "a.webp", "a.gif", "a.bmp", "a.ico", "a.tif", "a.tiff",
        ] {
            assert!(
                is_raster_thumbnail_supported(Path::new(ext)),
                "expected {ext} to be decoded in-process"
            );
            assert!(
                !is_shell_thumbnail_supported(Path::new(ext)),
                "expected {ext} not to be routed through the Shell"
            );
        }

        for ext in [
            "a.mp4", "a.mkv", "a.pdf", "a.docx", "a.svg", "a.heic", "a.cr2", "a.ttf", "a.glb",
            "a.3mf", "a.MP4", "a.PDF",
        ] {
            assert!(
                is_shell_thumbnail_supported(Path::new(ext)),
                "expected {ext} to be routed through the Shell"
            );
            assert!(
                !is_raster_thumbnail_supported(Path::new(ext)),
                "expected {ext} not to be decoded in-process"
            );
        }

        for rejected in ["a.txt", "a.rs", "a.exe", "no_ext"] {
            assert!(!is_raster_thumbnail_supported(Path::new(rejected)));
            assert!(!is_shell_thumbnail_supported(Path::new(rejected)));
            assert!(!is_thumbnail_supported(Path::new(rejected)));
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_routed_formats_fail_gracefully_without_shell_thumbnails() {
        let dir = TempDir::new().unwrap();
        let engine = ThumbnailEngine::new();

        // Empty containers cannot be thumbnailed, so the engine must report a preview error
        // rather than panicking or hanging.
        for name in ["empty.pdf", "empty.mp4", "empty.docx"] {
            let path = dir.path().join(name);
            fs::write(&path, b"").unwrap();
            match engine
                .load_thumbnail(path.clone(), 64, CancellationToken::new())
                .await
            {
                Ok(image) => {
                    // A Shell handler may still return artwork; the buffer must be well formed.
                    assert_eq!(image.rgba.len(), (image.width * image.height * 4) as usize);
                }
                Err(err) => assert!(
                    matches!(err, OnyxError::Preview { .. }),
                    "expected OnyxError::Preview for {}, got {err:?}",
                    path.display()
                ),
            }
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_route_ignores_the_raster_size_budget() {
        let dir = TempDir::new().unwrap();
        let engine = ThumbnailEngine::with_config(ThumbnailConfig {
            max_source_file_bytes: 4,
            ..ThumbnailConfig::default()
        });

        // A Shell-routed format larger than the raster budget is still attempted.
        let big_pdf = dir.path().join("big.pdf");
        fs::write(&big_pdf, vec![b'x'; 4096]).unwrap();
        let result = engine
            .load_thumbnail(big_pdf.clone(), 32, CancellationToken::new())
            .await;
        if let Err(err) = result {
            assert!(
                matches!(err, OnyxError::Preview { .. }),
                "unexpected error kind for oversized Shell-routed file: {err:?}"
            );
        }

        // The very same budget still rejects an in-process raster decode.
        let big_png = dir.path().join("big.png");
        fs::write(&big_png, vec![b'x'; 4096]).unwrap();
        let err = engine
            .load_thumbnail(big_png, 32, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, OnyxError::Preview { .. }));
    }

    #[tokio::test]
    async fn generates_png_and_jpeg_thumbnails_preserving_aspect_ratio() {
        let dir = TempDir::new().unwrap();
        let png_path = dir.path().join("wide.png");
        let jpg_path = dir.path().join("tall.jpg");

        let png_img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_fn(240, 120, |x, y| {
            Rgba([(x % 255) as u8, (y % 255) as u8, 180, 255])
        });
        png_img.save(&png_path).unwrap();

        let jpg_img: ImageBuffer<Rgb<u8>, Vec<u8>> =
            ImageBuffer::from_fn(100, 300, |x, y| Rgb([(x % 255) as u8, (y % 255) as u8, 90]));
        jpg_img.save(&jpg_path).unwrap();

        let engine = ThumbnailEngine::new();

        let png_thumb = engine
            .load_thumbnail(png_path, 64, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(png_thumb.width, 64);
        assert_eq!(png_thumb.height, 32);
        assert_eq!(png_thumb.rgba.len(), 64 * 32 * 4);

        let jpg_thumb = engine
            .load_thumbnail(jpg_path, 90, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(jpg_thumb.width, 30);
        assert_eq!(jpg_thumb.height, 90);
        assert_eq!(jpg_thumb.rgba.len(), 30 * 90 * 4);
    }

    #[tokio::test]
    async fn cache_hit_on_repeat_and_invalidation_when_file_changes() {
        let dir = TempDir::new().unwrap();
        let img_path = dir.path().join("mutable.png");

        let img1: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_fn(128, 64, |_, _| Rgba([255, 0, 0, 255]));
        img1.save(&img_path).unwrap();

        let engine = ThumbnailEngine::new();
        let first = engine
            .load_thumbnail(img_path.clone(), 64, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(first.width, 64);
        assert_eq!(first.height, 32);
        assert_eq!(engine.cache_stats(), (1, 64 * 32 * 4));

        let sig1 = read_file_signature(&img_path, &img_path).await.unwrap();
        let cached = engine
            .get_cached(&img_path, sig1.modified_secs, sig1.size, 64)
            .expect("should hit cache");
        assert_eq!(cached, first);

        // Also verify extended-length `\\?\` and uppercase path variants hit the same cache entry.
        let ext_variant = to_extended_length_path(&img_path);
        let cached_ext = engine
            .get_cached(&ext_variant, sig1.modified_secs, sig1.size, 64)
            .expect("extended-length path should hit normalized cache key");
        assert_eq!(cached_ext, first);

        // Overwrite file with different dimensions and size so signature changes.
        let img2: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_fn(80, 160, |x, y| {
            Rgba([(x % 250) as u8, (y % 250) as u8, 255, 255])
        });
        img2.save(&img_path).unwrap();

        let second = engine
            .load_thumbnail(img_path.clone(), 64, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(second.width, 32);
        assert_eq!(second.height, 64);
        assert_ne!(first, second);
        assert_eq!(engine.cache_stats().0, 2);

        engine.clear_cache();
        assert_eq!(engine.cache_stats(), (0, 0));
    }

    #[tokio::test]
    async fn byte_budget_and_entry_count_lru_eviction() {
        let dir = TempDir::new().unwrap();

        // Each 32x32 RGBA thumbnail is 32 * 32 * 4 = 4096 bytes.
        // Configure budget to hold at most 2 thumbnails (8192 bytes).
        let config = ThumbnailConfig {
            max_cache_entries: 10,
            max_cache_bytes: 8192,
            max_source_file_bytes: 5 * 1024 * 1024,
            max_concurrent_decodes: 2,
        };
        let engine = ThumbnailEngine::with_config(config);

        let mut paths = Vec::new();
        for i in 0..3u8 {
            let p = dir.path().join(format!("img_{i}.png"));
            let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
                ImageBuffer::from_fn(64, 64, |_, _| Rgba([i * 50, 100, 200, 255]));
            img.save(&p).unwrap();
            paths.push(p);
        }

        for p in &paths {
            engine
                .load_thumbnail(p.clone(), 32, CancellationToken::new())
                .await
                .unwrap();
        }

        let (entries, bytes) = engine.cache_stats();
        assert_eq!(entries, 2, "oldest entry should be evicted by byte budget");
        assert_eq!(bytes, 8192);

        let s0 = read_file_signature(&paths[0], &paths[0]).await.unwrap();
        let s1 = read_file_signature(&paths[1], &paths[1]).await.unwrap();
        let s2 = read_file_signature(&paths[2], &paths[2]).await.unwrap();

        assert!(
            engine
                .get_cached(&paths[0], s0.modified_secs, s0.size, 32)
                .is_none()
        );
        assert!(
            engine
                .get_cached(&paths[1], s1.modified_secs, s1.size, 32)
                .is_some()
        );
        assert!(
            engine
                .get_cached(&paths[2], s2.modified_secs, s2.size, 32)
                .is_some()
        );
    }

    #[test]
    fn cloud_placeholder_attribute_mask_rejects_unhydrated_files() {
        assert!(!is_cloud_placeholder_attributes(0x0000_0020)); // FILE_ATTRIBUTE_ARCHIVE
        assert!(is_cloud_placeholder_attributes(
            FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
        ));
        assert!(is_cloud_placeholder_attributes(
            FILE_ATTRIBUTE_RECALL_ON_OPEN
        ));
        assert!(is_cloud_placeholder_attributes(FILE_ATTRIBUTE_OFFLINE));
        assert!(is_cloud_placeholder_attributes(
            0x0000_0020 | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
        ));
    }

    #[test]
    fn normalized_thumbnail_key_folds_case_and_strips_verbatim_prefix() {
        let k1 = ThumbnailKey::new(
            Path::new(r"\\?\C:\Photos\Summer\IMG_01.PNG"),
            100,
            50,
            2048,
            64,
        );
        let k2 = ThumbnailKey::new(Path::new("c:/photos/summer/img_01.png"), 100, 50, 2048, 64);
        assert_eq!(k1, k2);
    }

    #[tokio::test]
    async fn rejects_cancelled_unsupported_and_oversized_files() {
        let dir = TempDir::new().unwrap();

        // Unsupported extension
        let txt_path = dir.path().join("notes.txt");
        fs::write(&txt_path, b"not an image").unwrap();
        let engine = ThumbnailEngine::new();
        let unsupported_err = engine
            .load_thumbnail(txt_path, 64, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(unsupported_err, OnyxError::Preview { .. }));

        // Oversized file rejection
        let small_limit_engine = ThumbnailEngine::with_config(ThumbnailConfig {
            max_source_file_bytes: 32,
            ..ThumbnailConfig::default()
        });
        let png_path = dir.path().join("oversized.png");
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_fn(64, 64, |x, y| Rgba([x as u8, y as u8, 50, 255]));
        img.save(&png_path).unwrap();

        let oversized_err = small_limit_engine
            .load_thumbnail(png_path.clone(), 32, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(oversized_err, OnyxError::Preview { .. }));

        // Pre-cancelled token
        let cancel = CancellationToken::new();
        cancel.cancel();
        let cancel_err = engine
            .load_thumbnail(png_path, 32, cancel)
            .await
            .unwrap_err();
        assert!(matches!(cancel_err, OnyxError::Cancelled));
    }
}
