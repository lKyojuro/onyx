//! Bounded LRU caches and SQLite metadata persistence for Onyx.

mod folder_size_store;
mod memory;
mod metadata;

pub use folder_size_store::{FolderSizeStore, dir_mtime_secs};
pub use memory::{
    CachedFolderSize, DEFAULT_DIRECTORY_CACHE_CAPACITY, DEFAULT_FOLDER_SIZE_CACHE_CAPACITY,
    DEFAULT_ICON_CACHE_CAPACITY, DEFAULT_THUMBNAIL_CACHE_CAPACITY, DirectoryCache, FolderSizeCache,
    IconCache, RgbaImageData, ThumbnailCache,
};
pub use metadata::{DirMetadataRecord, FolderVisit, MetadataStore};
