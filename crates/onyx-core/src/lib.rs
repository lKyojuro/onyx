//! Core domain types, natural sorting, path utilities, navigation state, and error definitions for Onyx.

pub mod error;
pub mod navigation;
pub mod path;
pub mod sort;
pub mod types;

pub use error::{OnyxError, OnyxResult};
pub use navigation::{NavHistory, TabManager, TabState};
pub use path::{
    BreadcrumbSegment, build_breadcrumbs, format_bytes, format_timestamp, infer_type_label,
    normalize_display_path, to_extended_length_path,
};
pub use sort::{SortColumn, SortDirection, SortSpec, natural_cmp, sort_entries};
pub use types::{
    DriveInfo, DriveKind, FavoriteItem, FileAttributes, FileEntry, FileKind, GenerationId,
    KnownFolder, KnownFolderKind, PaneId, ThemeMode, ViewMode,
};
