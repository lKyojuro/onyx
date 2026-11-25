//! Public value types for file operations, conflict policies, progress reporting, and results.

use crate::undo::UndoAction;
use std::path::PathBuf;

/// Strategy applied when a destination path already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ConflictPolicy {
    /// Leave the existing destination untouched and record the item as skipped.
    Skip,
    /// Replace the existing destination item (never overwriting a file with itself).
    Overwrite,
    /// Generate a unique sibling name such as `"name (2).ext"`.
    #[default]
    AutoRename,
    /// Fail the conflicting item (or operation) with an `AlreadyExists` error.
    Fail,
    /// Abort/cancel the operation immediately when a conflict is encountered.
    Cancel,
}

/// Interactive resolution chosen for an individual file or directory conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConflictResolution {
    /// Replace the existing destination item (never deleting a source when source == destination).
    Overwrite,
    /// Leave the existing destination untouched and skip this item.
    Skip,
    /// Keep both items by generating a unique Windows-Explorer-style sibling name (`name (2).ext`).
    KeepBoth,
    /// Abort the running operation cleanly.
    Cancel,
}

impl From<ConflictResolution> for ConflictPolicy {
    fn from(value: ConflictResolution) -> Self {
        match value {
            ConflictResolution::Overwrite => Self::Overwrite,
            ConflictResolution::Skip => Self::Skip,
            ConflictResolution::KeepBoth => Self::AutoRename,
            ConflictResolution::Cancel => Self::Cancel,
        }
    }
}

/// Decision returned by a [`ConflictResolver`] for a single conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConflictDecision {
    pub resolution: ConflictResolution,
    pub apply_to_all: bool,
}

impl ConflictDecision {
    /// Applies `resolution` only to the current conflict.
    #[must_use]
    pub const fn once(resolution: ConflictResolution) -> Self {
        Self {
            resolution,
            apply_to_all: false,
        }
    }

    /// Applies `resolution` to the current conflict and all remaining conflicts in this operation.
    #[must_use]
    pub const fn for_all(resolution: ConflictResolution) -> Self {
        Self {
            resolution,
            apply_to_all: true,
        }
    }
}

/// Contextual details about a destination collision passed to a [`ConflictResolver`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictInfo {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub is_dir: bool,
}

/// Callback interface allowing callers (such as a UI dialog) to resolve copy/move conflicts
/// dynamically per item.
pub trait ConflictResolver: Send {
    fn resolve(&mut self, info: &ConflictInfo) -> ConflictDecision;
}

impl<F> ConflictResolver for F
where
    F: FnMut(&ConflictInfo) -> ConflictDecision + Send,
{
    fn resolve(&mut self, info: &ConflictInfo) -> ConflictDecision {
        self(info)
    }
}

/// Kind of file operation tracked in progress notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperationKind {
    Copy,
    Move,
    Rename,
    CreateDir,
    Duplicate,
    Recycle,
    PermanentDelete,
    ExtractArchive,
    CreateArchive,
}

/// Explicit request required for irreversible permanent deletion.
///
/// [`FileOperationsEngine::permanent_delete`](crate::FileOperationsEngine::permanent_delete)
/// refuses to run unless `confirm_permanent` is `true`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermanentDeleteRequest {
    pub paths: Vec<PathBuf>,
    pub confirm_permanent: bool,
}

impl PermanentDeleteRequest {
    /// Creates an explicitly confirmed permanent deletion request.
    #[must_use]
    pub fn confirmed(paths: Vec<PathBuf>) -> Self {
        Self {
            paths,
            confirm_permanent: true,
        }
    }

    /// Creates an unconfirmed request (useful for staged UI flows or tests).
    #[must_use]
    pub fn unconfirmed(paths: Vec<PathBuf>) -> Self {
        Self {
            paths,
            confirm_permanent: false,
        }
    }
}

/// Incremental progress snapshot streamed to UI or background task observers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationProgress {
    pub op_id: u64,
    pub kind: OperationKind,
    pub current_item: String,
    pub completed_items: usize,
    pub total_items: usize,
    pub bytes_copied: u64,
    pub total_bytes: u64,
    pub finished: bool,
    pub cancelled: bool,
    pub error: Option<String>,
}

/// Summary report returned after an operation finishes (including partial failures).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OperationReport {
    pub succeeded: Vec<PathBuf>,
    pub failed: Vec<(PathBuf, String)>,
    pub skipped: Vec<PathBuf>,
    pub undo_action: Option<UndoAction>,
}
