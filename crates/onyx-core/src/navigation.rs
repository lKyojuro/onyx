use crate::sort::SortSpec;
use crate::types::GenerationId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const MAX_HISTORY_ENTRIES: usize = 128;

/// Bounded browser-style Back/Forward navigation history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavHistory {
    entries: Vec<PathBuf>,
    cursor: Option<usize>,
}

impl Default for NavHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl NavHistory {
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            cursor: None,
        }
    }

    #[must_use]
    pub fn with_initial(path: PathBuf) -> Self {
        Self {
            entries: vec![path],
            cursor: Some(0),
        }
    }

    pub fn push(&mut self, path: PathBuf) {
        if self.current() == Some(&path) {
            return;
        }
        if let Some(idx) = self.cursor {
            self.entries.truncate(idx + 1);
        }
        self.entries.push(path);
        if self.entries.len() > MAX_HISTORY_ENTRIES {
            let overflow = self.entries.len() - MAX_HISTORY_ENTRIES;
            self.entries.drain(0..overflow);
        }
        self.cursor = Some(self.entries.len() - 1);
    }

    #[must_use]
    pub fn current(&self) -> Option<&PathBuf> {
        self.cursor.and_then(|idx| self.entries.get(idx))
    }

    #[must_use]
    pub fn can_go_back(&self) -> bool {
        matches!(self.cursor, Some(idx) if idx > 0)
    }

    #[must_use]
    pub fn can_go_forward(&self) -> bool {
        matches!(self.cursor, Some(idx) if idx + 1 < self.entries.len())
    }

    pub fn go_back(&mut self) -> Option<&PathBuf> {
        if let Some(idx) = self.cursor
            && idx > 0
        {
            self.cursor = Some(idx - 1);
            return self.current();
        }
        None
    }

    pub fn go_forward(&mut self) -> Option<&PathBuf> {
        if let Some(idx) = self.cursor
            && idx + 1 < self.entries.len()
        {
            self.cursor = Some(idx + 1);
            return self.current();
        }
        None
    }
}

/// Per-tab navigation, selection, and sorting state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabState {
    pub id: u64,
    pub title: String,
    pub path: PathBuf,
    pub history: NavHistory,
    pub sort: SortSpec,
    pub selected_paths: Vec<PathBuf>,
    pub generation: GenerationId,
}

impl TabState {
    #[must_use]
    pub fn new(id: u64, path: PathBuf) -> Self {
        let title = derive_tab_title(&path);
        Self {
            id,
            title,
            history: NavHistory::with_initial(path.clone()),
            path,
            sort: SortSpec::default(),
            selected_paths: Vec::new(),
            generation: GenerationId::new(1),
        }
    }

    pub fn navigate(&mut self, path: PathBuf, record_history: bool) -> GenerationId {
        self.path = path.clone();
        self.title = derive_tab_title(&self.path);
        if record_history {
            self.history.push(path);
        }
        self.selected_paths.clear();
        self.generation = self.generation.next();
        self.generation
    }
}

/// Derives a concise tab title from a path.
#[must_use]
pub fn derive_tab_title(path: &std::path::Path) -> String {
    if let Some(name) = path.file_name().and_then(|n| n.to_str())
        && !name.is_empty()
    {
        return name.to_string();
    }
    let display = crate::path::normalize_display_path(path);
    if display.is_empty() {
        "Home".to_string()
    } else {
        display
    }
}

/// Multi-tab state manager with safe bounds and reordering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabManager {
    tabs: Vec<TabState>,
    active_index: usize,
    next_id: u64,
}

impl TabManager {
    #[must_use]
    pub fn new(initial_path: PathBuf) -> Self {
        Self {
            tabs: vec![TabState::new(1, initial_path)],
            active_index: 0,
            next_id: 2,
        }
    }

    #[must_use]
    pub fn tabs(&self) -> &[TabState] {
        &self.tabs
    }

    #[must_use]
    pub fn active_index(&self) -> usize {
        self.active_index
    }

    #[must_use]
    pub fn active_tab(&self) -> Option<&TabState> {
        self.tabs.get(self.active_index)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut TabState> {
        self.tabs.get_mut(self.active_index)
    }

    pub fn new_tab(&mut self, path: PathBuf) -> usize {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.tabs.push(TabState::new(id, path));
        self.active_index = self.tabs.len() - 1;
        self.active_index
    }

    pub fn switch_tab(&mut self, index: usize) -> bool {
        if index < self.tabs.len() {
            self.active_index = index;
            true
        } else {
            false
        }
    }

    /// Closes a tab if more than one tab is open. Returns `true` if closed.
    pub fn close_tab(&mut self, index: usize) -> bool {
        if self.tabs.len() <= 1 || index >= self.tabs.len() {
            return false;
        }
        self.tabs.remove(index);
        if self.active_index >= self.tabs.len() {
            self.active_index = self.tabs.len() - 1;
        } else if index < self.active_index {
            self.active_index -= 1;
        }
        true
    }

    /// Reorders a tab from `from` to `to`, keeping the active tab tracked cleanly.
    pub fn reorder_tab(&mut self, from: usize, to: usize) -> bool {
        if from >= self.tabs.len() || to >= self.tabs.len() || from == to {
            return false;
        }
        let active_id = self.tabs.get(self.active_index).map(|t| t.id);
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        if let Some(id) = active_id
            && let Some(pos) = self.tabs.iter().position(|t| t.id == id)
        {
            self.active_index = pos;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nav_history_back_forward_truncates_future() {
        let mut h = NavHistory::with_initial(PathBuf::from(r"C:\A"));
        h.push(PathBuf::from(r"C:\B"));
        h.push(PathBuf::from(r"C:\C"));
        assert!(h.can_go_back());
        assert!(!h.can_go_forward());

        assert_eq!(h.go_back(), Some(&PathBuf::from(r"C:\B")));
        assert!(h.can_go_forward());

        h.push(PathBuf::from(r"C:\D"));
        assert!(!h.can_go_forward());
        assert_eq!(h.current(), Some(&PathBuf::from(r"C:\D")));
    }

    #[test]
    fn tab_manager_lifecycle_and_reorder() {
        let mut mgr = TabManager::new(PathBuf::from(r"C:\One"));
        mgr.new_tab(PathBuf::from(r"C:\Two"));
        mgr.new_tab(PathBuf::from(r"C:\Three"));
        assert_eq!(mgr.tabs().len(), 3);
        assert_eq!(mgr.active_index(), 2);

        assert!(mgr.reorder_tab(2, 0));
        assert_eq!(mgr.active_index(), 0);
        assert_eq!(mgr.active_tab().map(|t| t.title.as_str()), Some("Three"));

        assert!(mgr.close_tab(0));
        assert_eq!(mgr.tabs().len(), 2);
    }
}
