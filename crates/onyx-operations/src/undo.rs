//! Bounded undo journal for reversible file operations.

use std::collections::VecDeque;
use std::path::PathBuf;

/// Default maximum number of operations retained in [`UndoJournal`].
pub const DEFAULT_UNDO_CAPACITY: usize = 32;

/// A single reversible file operation recorded in [`UndoJournal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoAction {
    /// Reverses a rename by moving `from` (current path) back to `to` (original path).
    ReverseRename { from: PathBuf, to: PathBuf },
    /// Removes a newly created empty directory.
    RemoveCreatedDir(PathBuf),
    /// Reverses a batch move: each `(from, to)` pair moves `from` (current) back to `to` (original).
    ReverseMoves(Vec<(PathBuf, PathBuf)>),
    /// Reverses a copy or duplicate by recycling the newly created target paths.
    RecycleCreatedCopies(Vec<PathBuf>),
}

/// Bounded LIFO stack of undoable operations (default capacity: 32 entries).
#[derive(Debug, Clone)]
pub struct UndoJournal {
    entries: VecDeque<UndoAction>,
    max_entries: usize,
}

impl Default for UndoJournal {
    fn default() -> Self {
        Self::new()
    }
}

impl UndoJournal {
    /// Creates an empty undo journal with the default capacity (32 entries).
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_UNDO_CAPACITY)
    }

    /// Creates an empty undo journal bounded to `max_entries`.
    #[must_use]
    pub fn with_capacity(max_entries: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(max_entries.min(DEFAULT_UNDO_CAPACITY)),
            max_entries,
        }
    }

    /// Pushes a new undoable action, evicting the oldest entry if the journal is full.
    pub fn push(&mut self, action: UndoAction) {
        if self.max_entries == 0 {
            return;
        }
        while self.entries.len() >= self.max_entries {
            self.entries.pop_front();
        }
        self.entries.push_back(action);
    }

    /// Pops the most recent undoable action (LIFO).
    pub fn pop(&mut self) -> Option<UndoAction> {
        self.entries.pop_back()
    }

    /// Returns a reference to the most recent undoable action without popping it.
    #[must_use]
    pub fn peek(&self) -> Option<&UndoAction> {
        self.entries.back()
    }

    /// Returns `true` if at least one undo action is available.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        !self.entries.is_empty()
    }

    /// Returns the number of stored undo actions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if no undo actions are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Clears all stored undo entries.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn undo_journal_enforces_32_entry_bound_and_lifo_order() {
        let mut journal = UndoJournal::new();
        assert!(!journal.can_undo());
        assert_eq!(journal.len(), 0);

        for idx in 0..40 {
            journal.push(UndoAction::RemoveCreatedDir(PathBuf::from(format!(
                "dir_{idx}"
            ))));
        }

        assert!(journal.can_undo());
        assert_eq!(journal.len(), DEFAULT_UNDO_CAPACITY);
        assert_eq!(
            journal.pop(),
            Some(UndoAction::RemoveCreatedDir(PathBuf::from("dir_39")))
        );

        let mut last = None;
        while let Some(item) = journal.pop() {
            last = Some(item);
        }
        // Entries 0..8 were evicted; oldest retained entry was dir_8.
        assert_eq!(
            last,
            Some(UndoAction::RemoveCreatedDir(PathBuf::from("dir_8")))
        );
        assert!(!journal.can_undo());
    }
}
