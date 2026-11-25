//! Centralized safe file operations engine for Onyx.
//!
//! All filesystem I/O executes off the caller/UI thread via `tokio::task::spawn_blocking`,
//! enforces data-integrity invariants (Recycle Bin default, explicit permanent-delete opt-in,
//! recursion guards, symlink/junction cycle safety, disk-space preflight, cancellation),
//! and maintains a bounded undo journal.

pub mod archive;
pub mod engine;
pub mod path_guard;
pub mod recycler;
pub mod types;
pub mod undo;

pub use archive::{
    ArchiveEntry, ArchiveFormat, ArchiveLimits, DEFAULT_MAX_ARCHIVE_ENTRIES,
    DEFAULT_MAX_ARCHIVE_UNCOMPRESSED_BYTES, DEFAULT_MAX_COMPRESSION_RATIO, create_archive,
    extract_archive, extract_archive_with_limits, is_supported_archive_path, list_archive_entries,
    list_archive_entries_with_limits,
};
pub use engine::FileOperationsEngine;
pub use path_guard::{
    generate_auto_rename_path, generate_keep_both_path, is_same_or_descendant, validate_item_name,
};
pub use recycler::{Recycler, SpaceChecker, WindowsRecycler, WindowsSpaceChecker};
pub use types::{
    ConflictDecision, ConflictInfo, ConflictPolicy, ConflictResolution, ConflictResolver,
    OperationKind, OperationProgress, OperationReport, PermanentDeleteRequest,
};
pub use undo::{DEFAULT_UNDO_CAPACITY, UndoAction, UndoJournal};
