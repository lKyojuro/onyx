//! Asynchronous filesystem engine and change watcher for Onyx.

mod engine;
mod folder_size;
mod watcher;

pub use engine::{DirBatch, EnumerationOptions, FsEngine};
pub use folder_size::{DEFAULT_MAX_FOLDER_SIZE_ENTRIES, FolderSizeResult, compute_folder_size};
pub use watcher::{DEFAULT_DEBOUNCE_DURATION, DirectoryWatcher, WatchEvent};
