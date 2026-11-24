//! Centralized safe file operations engine for Onyx.
//!
//! All filesystem I/O executes off the caller/UI thread via `tokio::task::spawn_blocking`,
//! enforces data-integrity invariants (Recycle Bin default, explicit permanent-delete opt-in,
//! recursion guards, symlink/junction cycle safety, disk-space preflight, cancellation),
//! and maintains a bounded undo journal.

pub mod engine;
pub mod path_guard;
pub mod recycler;
pub mod types;
pub mod undo;

pub use engine::FileOperationsEngine;
pub use path_guard::{generate_auto_rename_path, is_same_or_descendant, validate_item_name};
pub use recycler::{Recycler, SpaceChecker, WindowsRecycler, WindowsSpaceChecker};
pub use types::{
    ConflictPolicy, OperationKind, OperationProgress, OperationReport, PermanentDeleteRequest,
};
pub use undo::{DEFAULT_UNDO_CAPACITY, UndoAction, UndoJournal};
