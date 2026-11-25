//! Core domain types, natural sorting, path utilities, navigation state, and error definitions for Onyx.

pub mod error;
pub mod filter;
pub mod group;
pub mod navigation;
pub mod path;
pub mod sort;
pub mod types;

pub use error::{OnyxError, OnyxResult};
pub use filter::{
    FilteredEntry, FilteredEntryRef, FuzzyMatch, cmp_fuzzy_score_desc, filter_entries,
    filter_entry_refs, fuzzy_match,
};
pub use group::{
    DateBucket, EntryGroup, GroupByMode, SizeBucket, classify_date_bucket, classify_size_bucket,
    group_entries, group_entries_at,
};
pub use navigation::{NavHistory, TabManager, TabState};
pub use path::{
    BreadcrumbSegment, build_breadcrumbs, expand_env_vars, format_bytes, format_timestamp,
    infer_type_label, normalize_display_path, split_autocomplete_prefix, to_extended_length_path,
};
pub use sort::{SortColumn, SortDirection, SortSpec, natural_cmp, sort_entries};
pub use types::{
    DriveInfo, DriveKind, FavoriteItem, FileAttributes, FileEntry, FileKind, GenerationId,
    KnownFolder, KnownFolderKind, PaneId, ThemeMode, ViewMode,
};
