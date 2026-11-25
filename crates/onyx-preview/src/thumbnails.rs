//! Async, concurrency-bounded, byte-budgeted LRU thumbnail loader and cache.

use image::{GenericImageView, ImageFormat, ImageReader, Limits};
use lru::LruCache;
use onyx_core::{OnyxError, OnyxResult, format_bytes, to_extended_length_path};
use std::fs::File;
use std::io::{Cursor, Read};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Maximum untrusted image dimension accepted before rejecting decompression bombs.
const MAX_DECODE_DIMENSION: u32 = 16_384;
/// Maximum memory allocation allowed during a single thumbnail decode (128 MiB).
const MAX_DECODE_ALLOC_BYTES: u64 = 128 * 1024 * 1024;

/// Cache lookup key combining canonical path, modification timestamp, file size, and target size.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ThumbnailKey {
    pub path: PathBuf,
    pub modified_secs: i64,
    pub size: u64,
    pub target_px: u32,
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

/// Returns `true` if `path` has a supported raster image extension for thumbnail generation.
#[must_use]
pub fn is_thumbnail_supported(path: &Path) -> bool {
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

#[derive(Debug)]
struct CacheState {
    lru: LruCache<ThumbnailKey, ThumbnailImage>,
    current_bytes: usize,
    max_entries: usize,
    max_bytes: usize,
}

impl CacheState {
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        let cap = NonZeroUsize::new(max_entries.max(1)).unwrap_or(NonZeroUsize::MIN);
        Self {
            lru: LruCache::new(cap),
            current_bytes: 0,
            max_entries,
            max_bytes,
        }
    }

    fn get(&mut self, key: &ThumbnailKey) -> Option<ThumbnailImage> {
        self.lru.get(key).cloned()
    }

    fn insert(&mut self, key: ThumbnailKey, image: ThumbnailImage) {
        let entry_bytes = image.rgba.len();
        if self.max_entries == 0 || entry_bytes > self.max_bytes {
            return;
        }

        if let Some(old) = self.lru.pop(&key) {
            self.current_bytes = self.current_bytes.saturating_sub(old.rgba.len());
        }

        while self.lru.len() >= self.max_entries
            || self.current_bytes.saturating_add(entry_bytes) > self.max_bytes
        {
            let Some((_, evicted)) = self.lru.pop_lru() else {
                break;
            };
            self.current_bytes = self.current_bytes.saturating_sub(evicted.rgba.len());
        }

        self.current_bytes = self.current_bytes.saturating_add(entry_bytes);
        self.lru.put(key, image);
    }

    fn clear(&mut self) {
        self.lru.clear();
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
        let key = ThumbnailKey {
            path: path.to_path_buf(),
            modified_secs,
            size,
            target_px,
        };
        let mut guard = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.get(&key)
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

        if !is_thumbnail_supported(&path) {
            return Err(OnyxError::Preview {
                message: format!("Unsupported thumbnail extension for '{}'", path.display()),
            });
        }

        let ext_path = to_extended_length_path(&path);
        let (modified_secs, size) = read_file_signature(&path, &ext_path)?;

        if size > self.config.max_source_file_bytes {
            return Err(OnyxError::Preview {
                message: format!(
                    "Image size ({}) exceeds thumbnail limit ({})",
                    format_bytes(size),
                    format_bytes(self.config.max_source_file_bytes)
                ),
            });
        }

        if let Some(cached) = self.get_cached(&path, modified_secs, size, target_px) {
            return Ok(cached);
        }

        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        let permit = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                return Err(OnyxError::Cancelled);
            }
            res = Arc::clone(&self.semaphore).acquire_owned() => {
                res.map_err(|_| OnyxError::Preview {
                    message: "Thumbnail concurrency semaphore closed".to_string(),
                })?
            }
        };

        if cancel.is_cancelled() {
            drop(permit);
            return Err(OnyxError::Cancelled);
        }

        // Re-check cache after acquiring semaphore permit in case a concurrent task populated it.
        if let Some(cached) = self.get_cached(&path, modified_secs, size, target_px) {
            drop(permit);
            return Ok(cached);
        }

        let max_source_bytes = self.config.max_source_file_bytes;
        let worker_cancel = cancel.clone();
        let decode_path = ext_path;

        let join_result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            decode_thumbnail_blocking(
                &decode_path,
                size,
                max_source_bytes,
                target_px,
                &worker_cancel,
            )
        })
        .await;

        let thumbnail = match join_result {
            Ok(Ok(img)) => img,
            Ok(Err(err)) => return Err(err),
            Err(join_err) => {
                return Err(OnyxError::Preview {
                    message: format!("Thumbnail worker task failed: {join_err}"),
                });
            }
        };

        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        let key = ThumbnailKey {
            path,
            modified_secs,
            size,
            target_px,
        };
        {
            let mut guard = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.insert(key, thumbnail.clone());
        }

        Ok(thumbnail)
    }
}

fn read_file_signature(original_path: &Path, ext_path: &Path) -> OnyxResult<(i64, u64)> {
    let meta = match std::fs::metadata(ext_path) {
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

    if !meta.is_file() {
        return Err(OnyxError::Preview {
            message: format!("'{}' is not a regular file", original_path.display()),
        });
    }

    let modified_secs = meta
        .modified()
        .ok()
        .and_then(|mtime| match mtime.duration_since(UNIX_EPOCH) {
            Ok(dur) => i64::try_from(dur.as_secs()).ok(),
            Err(err) => i64::try_from(err.duration().as_secs()).ok().map(|s| -s),
        })
        .unwrap_or(0);

    Ok((modified_secs, meta.len()))
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
        for unsupported in ["a.txt", "a.rs", "a.pdf", "a.mp4", "no_ext"] {
            assert!(
                !is_thumbnail_supported(Path::new(unsupported)),
                "expected {unsupported} to be rejected"
            );
        }
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

        let (mtime1, size1) = read_file_signature(&img_path, &img_path).unwrap();
        let cached = engine
            .get_cached(&img_path, mtime1, size1, 64)
            .expect("should hit cache");
        assert_eq!(cached, first);

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

        let (m0, s0) = read_file_signature(&paths[0], &paths[0]).unwrap();
        let (m1, s1) = read_file_signature(&paths[1], &paths[1]).unwrap();
        let (m2, s2) = read_file_signature(&paths[2], &paths[2]).unwrap();

        assert!(engine.get_cached(&paths[0], m0, s0, 32).is_none());
        assert!(engine.get_cached(&paths[1], m1, s1, 32).is_some());
        assert!(engine.get_cached(&paths[2], m2, s2, 32).is_some());
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
