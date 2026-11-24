use lru::LruCache;
use onyx_core::FileEntry;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Default maximum number of cached directory listings.
pub const DEFAULT_DIRECTORY_CACHE_CAPACITY: usize = 64;
/// Default maximum number of cached icons.
pub const DEFAULT_ICON_CACHE_CAPACITY: usize = 512;
/// Default maximum number of cached thumbnails.
pub const DEFAULT_THUMBNAIL_CACHE_CAPACITY: usize = 128;

/// Decoded RGBA8 image buffer used by icon and thumbnail caches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaImageData {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl RgbaImageData {
    /// Creates a new RGBA image buffer.
    #[must_use]
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Self {
        Self {
            width,
            height,
            pixels,
        }
    }

    /// Returns the total byte length of the RGBA pixel buffer.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.pixels.len()
    }
}

fn non_zero_capacity(capacity: usize) -> NonZeroUsize {
    NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN)
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Thread-safe bounded LRU cache for directory enumeration listings.
pub struct DirectoryCache {
    inner: Mutex<LruCache<PathBuf, Vec<FileEntry>>>,
}

impl DirectoryCache {
    /// Creates a directory cache with the given entry capacity (minimum 1).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(non_zero_capacity(capacity))),
        }
    }

    /// Retrieves a cached directory listing, promoting it to most-recently-used.
    #[must_use]
    pub fn get(&self, path: &Path) -> Option<Vec<FileEntry>> {
        let mut guard = lock_unpoisoned(&self.inner);
        guard.get(path).cloned()
    }

    /// Inserts or updates a directory listing in the cache.
    pub fn put(&self, path: PathBuf, entries: Vec<FileEntry>) {
        let mut guard = lock_unpoisoned(&self.inner);
        guard.put(path, entries);
    }

    /// Removes a specific directory from the cache. Returns `true` if it was present.
    pub fn invalidate(&self, path: &Path) -> bool {
        let mut guard = lock_unpoisoned(&self.inner);
        guard.pop(path).is_some()
    }

    /// Clears all cached directory listings.
    pub fn clear(&self) {
        let mut guard = lock_unpoisoned(&self.inner);
        guard.clear();
    }

    /// Number of directories currently cached.
    #[must_use]
    pub fn len(&self) -> usize {
        let guard = lock_unpoisoned(&self.inner);
        guard.len()
    }

    /// Returns `true` when no directories are cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Maximum number of directories this cache can hold.
    #[must_use]
    pub fn capacity(&self) -> usize {
        let guard = lock_unpoisoned(&self.inner);
        guard.cap().get()
    }
}

impl Default for DirectoryCache {
    fn default() -> Self {
        Self::new(DEFAULT_DIRECTORY_CACHE_CAPACITY)
    }
}

impl fmt::Debug for DirectoryCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectoryCache")
            .field("len", &self.len())
            .field("capacity", &self.capacity())
            .finish()
    }
}

/// Thread-safe bounded LRU cache for file icons keyed by extension, kind, or path.
pub struct IconCache {
    inner: Mutex<LruCache<String, RgbaImageData>>,
}

impl IconCache {
    /// Creates an icon cache with the given capacity (minimum 1).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(non_zero_capacity(capacity))),
        }
    }

    /// Retrieves a cached icon, promoting it to most-recently-used.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<RgbaImageData> {
        let mut guard = lock_unpoisoned(&self.inner);
        guard.get(key).cloned()
    }

    /// Inserts or updates an icon in the cache.
    pub fn put(&self, key: String, image: RgbaImageData) {
        let mut guard = lock_unpoisoned(&self.inner);
        guard.put(key, image);
    }

    /// Removes a specific icon key from the cache. Returns `true` if it was present.
    pub fn invalidate(&self, key: &str) -> bool {
        let mut guard = lock_unpoisoned(&self.inner);
        guard.pop(key).is_some()
    }

    /// Clears all cached icons.
    pub fn clear(&self) {
        let mut guard = lock_unpoisoned(&self.inner);
        guard.clear();
    }

    /// Number of icons currently cached.
    #[must_use]
    pub fn len(&self) -> usize {
        let guard = lock_unpoisoned(&self.inner);
        guard.len()
    }

    /// Returns `true` when no icons are cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Maximum number of icons this cache can hold.
    #[must_use]
    pub fn capacity(&self) -> usize {
        let guard = lock_unpoisoned(&self.inner);
        guard.cap().get()
    }
}

impl Default for IconCache {
    fn default() -> Self {
        Self::new(DEFAULT_ICON_CACHE_CAPACITY)
    }
}

impl fmt::Debug for IconCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IconCache")
            .field("len", &self.len())
            .field("capacity", &self.capacity())
            .finish()
    }
}

/// Thread-safe bounded LRU cache for file thumbnails keyed by file path.
pub struct ThumbnailCache {
    inner: Mutex<LruCache<PathBuf, RgbaImageData>>,
}

impl ThumbnailCache {
    /// Creates a thumbnail cache with the given capacity (minimum 1).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(non_zero_capacity(capacity))),
        }
    }

    /// Retrieves a cached thumbnail, promoting it to most-recently-used.
    #[must_use]
    pub fn get(&self, path: &Path) -> Option<RgbaImageData> {
        let mut guard = lock_unpoisoned(&self.inner);
        guard.get(path).cloned()
    }

    /// Inserts or updates a thumbnail in the cache.
    pub fn put(&self, path: PathBuf, image: RgbaImageData) {
        let mut guard = lock_unpoisoned(&self.inner);
        guard.put(path, image);
    }

    /// Removes a specific thumbnail from the cache. Returns `true` if it was present.
    pub fn invalidate(&self, path: &Path) -> bool {
        let mut guard = lock_unpoisoned(&self.inner);
        guard.pop(path).is_some()
    }

    /// Clears all cached thumbnails.
    pub fn clear(&self) {
        let mut guard = lock_unpoisoned(&self.inner);
        guard.clear();
    }

    /// Number of thumbnails currently cached.
    #[must_use]
    pub fn len(&self) -> usize {
        let guard = lock_unpoisoned(&self.inner);
        guard.len()
    }

    /// Returns `true` when no thumbnails are cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Maximum number of thumbnails this cache can hold.
    #[must_use]
    pub fn capacity(&self) -> usize {
        let guard = lock_unpoisoned(&self.inner);
        guard.cap().get()
    }
}

impl Default for ThumbnailCache {
    fn default() -> Self {
        Self::new(DEFAULT_THUMBNAIL_CACHE_CAPACITY)
    }
}

impl fmt::Debug for ThumbnailCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThumbnailCache")
            .field("len", &self.len())
            .field("capacity", &self.capacity())
            .finish()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use onyx_core::{FileAttributes, FileKind};

    fn sample_entry(name: &str) -> FileEntry {
        FileEntry {
            path: PathBuf::from(name),
            name: name.to_string(),
            extension: Some("txt".to_string()),
            kind: FileKind::File,
            size: 42,
            modified: Some(1_700_000_000),
            created: Some(1_700_000_000),
            attributes: FileAttributes::default(),
            is_symlink: false,
            symlink_target: None,
            type_label: "Text Document".to_string(),
        }
    }

    #[test]
    fn directory_cache_eviction_and_invalidation() {
        let cache = DirectoryCache::new(2);
        assert_eq!(cache.capacity(), 2);
        assert!(cache.is_empty());

        let d1 = PathBuf::from(r"C:\One");
        let d2 = PathBuf::from(r"C:\Two");
        let d3 = PathBuf::from(r"C:\Three");

        cache.put(d1.clone(), vec![sample_entry("a.txt")]);
        cache.put(d2.clone(), vec![sample_entry("b.txt")]);
        assert_eq!(cache.len(), 2);

        // Touch d1 so d2 becomes least-recently-used.
        assert!(cache.get(&d1).is_some());

        cache.put(d3.clone(), vec![sample_entry("c.txt")]);
        assert_eq!(cache.len(), 2);
        assert!(cache.get(&d2).is_none());
        assert!(cache.get(&d1).is_some());
        assert!(cache.get(&d3).is_some());

        assert!(cache.invalidate(&d1));
        assert!(!cache.invalidate(&d1));
        assert_eq!(cache.len(), 1);

        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn icon_and_thumbnail_caches_behave_as_lru() {
        let icons = IconCache::new(2);
        let img = RgbaImageData::new(2, 2, vec![255; 16]);
        assert_eq!(img.byte_len(), 16);

        icons.put("rs".to_string(), img.clone());
        icons.put("toml".to_string(), img.clone());
        icons.put("md".to_string(), img.clone());
        assert_eq!(icons.len(), 2);
        assert!(icons.get("rs").is_none());
        assert_eq!(icons.get("md"), Some(img.clone()));
        icons.clear();
        assert!(icons.is_empty());

        let thumbs = ThumbnailCache::new(2);
        let p1 = PathBuf::from(r"C:\a.png");
        let p2 = PathBuf::from(r"C:\b.png");
        thumbs.put(p1.clone(), img.clone());
        thumbs.put(p2.clone(), img.clone());
        assert_eq!(thumbs.get(&p1), Some(img));
        assert!(thumbs.invalidate(&p1));
        assert!(thumbs.get(&p1).is_none());
        thumbs.clear();
        assert!(thumbs.is_empty());
    }
}
