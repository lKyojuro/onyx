use onyx_cache::{DirectoryCache, FolderSizeCache, IconCache};
use onyx_config::{
    AppConfig, ConfigStore, DEFAULT_PANE_SPLIT_RATIO, DEFAULT_SIDEBAR_WIDTH, PreviewSide,
    SessionState, VALID_SIDEBAR_GROUPS, clamp_pane_split_ratio, clamp_preview_width,
    clamp_sidebar_width, sanitize_collapsed_sidebar_groups,
};
use onyx_core::{FileEntry, GenerationId, PaneId, TabManager, TabState, ThemeMode};
use onyx_operations::FileOperationsEngine;
use onyx_preview::ThumbnailEngine;
use onyx_search::{SearchIndex, UnifiedSearchEngine};
use onyx_ui::UiFileEntry;
use onyx_windows::DropTargetGuard;
use slint::VecModel;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::operations::Prompt;

/// Maximum number of root directories watched concurrently by [`IncrementalIndexer`].
pub(crate) const MAX_WATCHED_INDEX_ROOTS: usize = 8;

#[derive(Default)]
pub(crate) struct Pane {
    pub generation: GenerationId,
    pub cancel: CancellationToken,
    pub thumb_cancel: CancellationToken,
    pub folder_size_cancel: CancellationToken,
    pub loaded_path: Option<PathBuf>,
    pub entries: Vec<FileEntry>,
    pub model: Rc<VecModel<UiFileEntry>>,
    pub loading: bool,
    pub error: String,
    pub query: String,
    pub result_order: bool,
    pub scroll_offset: f32,
}

impl Pane {
    pub fn restart(&mut self) -> (GenerationId, CancellationToken) {
        self.cancel.cancel();
        self.thumb_cancel.cancel();
        self.folder_size_cancel.cancel();
        self.generation = self.generation.next();
        self.cancel = CancellationToken::new();
        self.thumb_cancel = self.cancel.child_token();
        self.folder_size_cancel = self.cancel.child_token();
        self.loading = true;
        self.error.clear();
        (self.generation, self.cancel.clone())
    }

    pub fn restart_thumbnails(&mut self) -> (GenerationId, CancellationToken) {
        self.thumb_cancel.cancel();
        self.thumb_cancel = self.cancel.child_token();
        (self.generation, self.thumb_cancel.clone())
    }

    pub fn restart_folder_sizes(&mut self) -> (GenerationId, CancellationToken) {
        self.folder_size_cancel.cancel();
        self.folder_size_cancel = self.cancel.child_token();
        (self.generation, self.folder_size_cancel.clone())
    }
}

pub(crate) struct State {
    pub store: ConfigStore,
    pub config: AppConfig,
    pub tabs: TabManager,
    pub secondary_tab: TabState,
    pub panes: [Pane; 2],
    pub tab_scroll_offsets: HashMap<u64, f32>,
    pub active: PaneId,
    pub home: PathBuf,
    pub dual: bool,
    pub sidebar_visible: bool,
    pub sidebar_width: f32,
    pub preview_width: Option<f32>,
    pub pane_split_ratio: f32,
    pub collapsed_sidebar_groups: Vec<String>,
    pub preview_side: PreviewSide,
    pub quick_look: bool,
    pub directories: DirectoryCache,
    pub folder_sizes: FolderSizeCache,
    pub icons: IconCache,
    pub pending_icons: HashSet<String>,
    pub missing_icons: HashSet<String>,
    pub thumbnails: ThumbnailEngine,
    pub preview_cancel: CancellationToken,
    pub preview_path: Option<PathBuf>,
    pub search: UnifiedSearchEngine,
    pub search_index: Option<Arc<Mutex<SearchIndex>>>,
    pub indexer_generation: GenerationId,
    pub watched_index_roots: VecDeque<PathBuf>,
    pub index_path: PathBuf,
    pub indexed_search: bool,
    pub indexed_roots: HashSet<PathBuf>,
    pub indexing: Option<(PathBuf, CancellationToken)>,
    pub operations: Arc<FileOperationsEngine>,
    pub operation: Option<(CancellationToken, JoinHandle<()>)>,
    pub prompt: Option<Prompt>,
    pub archive_path: Option<PathBuf>,
    pub archive_cancel: CancellationToken,
    pub drop_target: Option<DropTargetGuard>,
    pub external_drag_active: bool,
}

impl State {
    pub fn new(
        store: ConfigStore,
        config: AppConfig,
        mut session: SessionState,
        home: PathBuf,
        path: Option<PathBuf>,
    ) -> Self {
        session.sanitize();
        let has_session = !session.tabs.is_empty();
        let mut paths = session.tabs.into_iter();
        let mut tabs = TabManager::new(paths.next().unwrap_or_else(|| home.clone()));
        for path in paths {
            tabs.new_tab(path);
        }
        tabs.switch_tab(session.active_tab.min(tabs.tabs().len().saturating_sub(1)));
        if let Some(path) = path
            && let Some(tab) = tabs.active_tab_mut()
        {
            tab.navigate(path, true);
        }
        let secondary = session.secondary_path.unwrap_or_else(|| home.clone());
        let dual = if has_session {
            session.dual_pane
        } else {
            config.dual_pane_default
        };
        let index_path = store.dir().join("cache").join("search.sqlite");
        Self {
            store,
            config,
            tabs,
            secondary_tab: TabState::new(0, secondary),
            panes: std::array::from_fn(|_| Pane::default()),
            tab_scroll_offsets: HashMap::new(),
            active: PaneId::Primary,
            home,
            dual,
            sidebar_visible: session.sidebar_visible,
            sidebar_width: clamp_sidebar_width(session.sidebar_width),
            preview_width: clamp_preview_width(session.preview_width),
            pane_split_ratio: clamp_pane_split_ratio(session.pane_split_ratio),
            collapsed_sidebar_groups: sanitize_collapsed_sidebar_groups(
                session.collapsed_sidebar_groups,
            ),
            preview_side: session.preview_side,
            quick_look: false,
            directories: DirectoryCache::default(),
            folder_sizes: FolderSizeCache::default(),
            icons: IconCache::default(),
            pending_icons: HashSet::new(),
            missing_icons: HashSet::new(),
            thumbnails: ThumbnailEngine::new(),
            preview_cancel: CancellationToken::new(),
            preview_path: None,
            search: UnifiedSearchEngine::live_only(),
            search_index: None,
            indexer_generation: GenerationId::default(),
            watched_index_roots: VecDeque::new(),
            index_path,
            indexed_search: false,
            indexed_roots: HashSet::new(),
            indexing: None,
            operations: Arc::new(FileOperationsEngine::new()),
            operation: None,
            prompt: None,
            archive_path: None,
            archive_cancel: CancellationToken::new(),
            drop_target: None,
            external_drag_active: false,
        }
    }

    pub fn pane(&self, pane: PaneId) -> &Pane {
        &self.panes[pane_number(pane)]
    }

    pub fn pane_mut(&mut self, pane: PaneId) -> &mut Pane {
        &mut self.panes[pane_number(pane)]
    }

    pub fn tab(&self, pane: PaneId) -> Option<&TabState> {
        match pane {
            PaneId::Primary => self.tabs.active_tab(),
            PaneId::Secondary => Some(&self.secondary_tab),
        }
    }

    pub fn tab_mut(&mut self, pane: PaneId) -> Option<&mut TabState> {
        match pane {
            PaneId::Primary => self.tabs.active_tab_mut(),
            PaneId::Secondary => Some(&mut self.secondary_tab),
        }
    }

    pub fn path(&self, pane: PaneId) -> &Path {
        self.tab(pane).map_or(&self.home, |tab| &tab.path)
    }

    pub fn selected(&self, pane: PaneId) -> Option<&FileEntry> {
        let path = self.tab(pane)?.selected_paths.first()?;
        self.pane(pane)
            .entries
            .iter()
            .find(|entry| &entry.path == path)
    }

    pub fn selected_paths(&self, pane: PaneId) -> Vec<PathBuf> {
        self.tab(pane)
            .map(|tab| tab.selected_paths.clone())
            .unwrap_or_default()
    }

    pub fn visible_panes(&self) -> impl Iterator<Item = PaneId> + use<> {
        [PaneId::Primary, PaneId::Secondary]
            .into_iter()
            .take(if self.dual { 2 } else { 1 })
    }

    pub fn index_exclusions(&self) -> Vec<PathBuf> {
        let mut exclusions = self.config.excluded_index_dirs.clone();
        if let Some(cache_dir) = self.index_path.parent() {
            exclusions.push(cache_dir.to_path_buf());
        }
        exclusions
    }

    pub fn toggle_sidebar(&mut self) -> bool {
        self.sidebar_visible = !self.sidebar_visible;
        self.sidebar_visible
    }

    pub fn toggle_preview_side(&mut self) -> PreviewSide {
        self.preview_side = self.preview_side.toggled();
        self.preview_side
    }

    pub fn is_sidebar_group_collapsed(&self, group_id: &str) -> bool {
        self.collapsed_sidebar_groups
            .iter()
            .any(|id| id == group_id)
    }

    pub fn set_sidebar_group_collapsed(&mut self, group_id: &str, collapsed: bool) {
        if !VALID_SIDEBAR_GROUPS.contains(&group_id) {
            return;
        }
        if collapsed {
            if !self.is_sidebar_group_collapsed(group_id) {
                self.collapsed_sidebar_groups.push(group_id.to_string());
            }
        } else {
            self.collapsed_sidebar_groups.retain(|id| id != group_id);
        }
    }

    pub fn toggle_sidebar_group(&mut self, group_id: &str) -> bool {
        let next = !self.is_sidebar_group_collapsed(group_id);
        self.set_sidebar_group_collapsed(group_id, next);
        self.is_sidebar_group_collapsed(group_id)
    }

    /// Resets layout visibility, widths, split ratio, sidebar groups, and preview dock side
    /// to their defaults while preserving tabs, paths, history, and dual-pane mode.
    pub fn reset_layout(&mut self) {
        self.sidebar_visible = true;
        self.config.show_preview_pane = true;
        self.sidebar_width = DEFAULT_SIDEBAR_WIDTH;
        self.preview_width = None;
        self.pane_split_ratio = DEFAULT_PANE_SPLIT_RATIO;
        self.collapsed_sidebar_groups.clear();
        self.preview_side = PreviewSide::Right;
    }

    /// Swaps the primary active tab with `secondary_tab`, preserving path, history, selection,
    /// sort, scroll offset, and flipping active pane focus. No-op when not in dual-pane mode.
    pub fn swap_panes(&mut self) -> bool {
        if !self.dual {
            return false;
        }
        let Some(primary_tab) = self.tabs.active_tab_mut() else {
            return false;
        };
        std::mem::swap(primary_tab, &mut self.secondary_tab);
        let primary_scroll = self.panes[0].scroll_offset;
        let secondary_scroll = self.panes[1].scroll_offset;
        self.panes[0].scroll_offset = secondary_scroll;
        self.panes[1].scroll_offset = primary_scroll;
        self.tab_scroll_offsets
            .insert(primary_tab.id, self.panes[0].scroll_offset);
        self.active = match self.active {
            PaneId::Primary => PaneId::Secondary,
            PaneId::Secondary => PaneId::Primary,
        };
        true
    }

    /// Moves a tab to the other pane, preserving history, selection, sort, and scroll offset.
    ///
    /// - If `primary_tab_index` is `Some(idx)`, moves that tab from the primary `TabManager`
    ///   to `secondary_tab` (swapping if it is the sole primary tab).
    /// - If `primary_tab_index` is `None`, moves whichever pane is currently `self.active`:
    ///   primary active tab -> secondary pane, or `secondary_tab` -> new active primary tab.
    /// - Gracefully no-ops and returns `false` when not in dual-pane mode.
    pub fn move_tab_to_other_pane(&mut self, primary_tab_index: Option<usize>) -> bool {
        if !self.dual {
            return false;
        }

        let source_primary_index = match primary_tab_index {
            Some(idx) => Some(idx),
            None if self.active == PaneId::Primary => Some(self.tabs.active_index()),
            None => None,
        };

        if let Some(index) = source_primary_index {
            if index >= self.tabs.tabs().len() {
                return false;
            }
            if self.tabs.tabs().len() <= 1 {
                // Keep at least one tab in the primary TabManager by swapping with secondary.
                let swapped = self.swap_panes();
                if swapped {
                    self.active = PaneId::Secondary;
                }
                return swapped;
            }

            let was_active_primary = index == self.tabs.active_index();
            let Some(moved) = self.tabs.tabs().get(index).cloned() else {
                return false;
            };
            let moved_scroll = if was_active_primary {
                self.panes[0].scroll_offset
            } else {
                self.tab_scroll_offsets
                    .get(&moved.id)
                    .copied()
                    .unwrap_or(0.0)
            };
            self.tab_scroll_offsets.remove(&moved.id);

            if !self.tabs.close_tab(index) {
                return false;
            }

            if was_active_primary {
                let next_primary_scroll = self
                    .tabs
                    .active_tab()
                    .and_then(|tab| self.tab_scroll_offsets.get(&tab.id).copied())
                    .unwrap_or(0.0);
                self.panes[0].scroll_offset = next_primary_scroll;
            }

            self.secondary_tab = moved;
            self.panes[1].scroll_offset = moved_scroll;
            self.active = PaneId::Secondary;
            true
        } else {
            // Active pane is Secondary: move `secondary_tab` into the primary `TabManager`.
            if let Some(prev_primary) = self.tabs.active_tab() {
                self.tab_scroll_offsets
                    .insert(prev_primary.id, self.panes[0].scroll_offset);
            }
            let moved = self.secondary_tab.clone();
            let moved_scroll = self.panes[1].scroll_offset;
            self.tabs.new_tab(moved.path.clone());
            if let Some(dst) = self.tabs.active_tab_mut() {
                let new_id = dst.id;
                *dst = moved;
                dst.id = new_id;
                self.tab_scroll_offsets.insert(new_id, moved_scroll);
            }
            self.panes[0].scroll_offset = moved_scroll;
            self.active = PaneId::Primary;
            true
        }
    }

    /// Moves the active pane's active tab to the other pane.
    pub fn move_active_tab_to_other_pane(&mut self) -> bool {
        self.move_tab_to_other_pane(None)
    }

    pub fn session(&self, width: u32, height: u32) -> SessionState {
        SessionState {
            tabs: self
                .tabs
                .tabs()
                .iter()
                .map(|tab| tab.path.clone())
                .collect(),
            active_tab: self.tabs.active_index(),
            dual_pane: self.dual,
            secondary_path: Some(self.secondary_tab.path.clone()),
            window_width: width,
            window_height: height,
            sidebar_visible: self.sidebar_visible,
            sidebar_width: clamp_sidebar_width(self.sidebar_width),
            preview_width: clamp_preview_width(self.preview_width),
            pane_split_ratio: clamp_pane_split_ratio(self.pane_split_ratio),
            collapsed_sidebar_groups: sanitize_collapsed_sidebar_groups(
                self.collapsed_sidebar_groups.clone(),
            ),
            preview_side: self.preview_side,
        }
    }
}

pub(crate) fn is_zip_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
}

pub(crate) const fn pane_number(pane: PaneId) -> usize {
    match pane {
        PaneId::Primary => 0,
        PaneId::Secondary => 1,
    }
}

pub(crate) fn pane_id(value: i32) -> PaneId {
    if value == 1 {
        PaneId::Secondary
    } else {
        PaneId::Primary
    }
}

pub(crate) fn effective_theme(theme: ThemeMode) -> ThemeMode {
    match theme {
        ThemeMode::System => onyx_windows::detect_system_theme(),
        explicit => explicit,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn pane_restart_and_thumbnail_cancellation_track_generations() {
        let mut pane = Pane::default();
        let (g1, c1) = pane.restart();
        assert_eq!(g1, GenerationId::new(1));
        assert!(!c1.is_cancelled());

        let (tg1, tc1) = pane.restart_thumbnails();
        assert_eq!(tg1, g1);
        assert!(!tc1.is_cancelled());

        // Restarting thumbnails cancels prior thumbnail token without cancelling directory load.
        let (tg2, tc2) = pane.restart_thumbnails();
        assert_eq!(tg2, g1);
        assert!(tc1.is_cancelled());
        assert!(!c1.is_cancelled());
        assert!(!tc2.is_cancelled());

        let (fg1, fc1) = pane.restart_folder_sizes();
        assert_eq!(fg1, g1);
        assert!(!fc1.is_cancelled());

        // Restarting the pane increments generation and cancels directory, thumbnail, and folder size tokens.
        let (g2, c2) = pane.restart();
        assert_eq!(g2, GenerationId::new(2));
        assert!(c1.is_cancelled());
        assert!(tc2.is_cancelled());
        assert!(fc1.is_cancelled());
        assert!(!c2.is_cancelled());
    }

    #[test]
    fn zip_extension_detection_is_case_insensitive() {
        assert!(is_zip_path(Path::new(r"C:\archive.zip")));
        assert!(is_zip_path(Path::new(r"C:\ARCHIVE.ZIP")));
        assert!(!is_zip_path(Path::new(r"C:\archive.tar.gz")));
        assert!(!is_zip_path(Path::new(r"C:\zip")));
    }

    fn test_state(dual: bool) -> State {
        let store = ConfigStore::with_dir(PathBuf::from("."));
        let config = AppConfig::default();
        let session = SessionState {
            tabs: vec![PathBuf::from(r"C:\PrimaryA")],
            active_tab: 0,
            dual_pane: dual,
            secondary_path: Some(PathBuf::from(r"D:\Secondary")),
            ..SessionState::default()
        };
        State::new(store, config, session, PathBuf::from(r"C:\Home"), None)
    }

    #[test]
    fn swap_panes_no_op_in_single_pane_mode() {
        let mut state = test_state(false);
        assert!(!state.swap_panes());
        assert_eq!(state.path(PaneId::Primary), Path::new(r"C:\PrimaryA"));
        assert_eq!(state.path(PaneId::Secondary), Path::new(r"D:\Secondary"));
        assert_eq!(state.active, PaneId::Primary);
    }

    #[test]
    fn swap_panes_exchanges_path_history_selection_scroll_and_focus() {
        let mut state = test_state(true);
        state
            .tab_mut(PaneId::Primary)
            .unwrap()
            .navigate(PathBuf::from(r"C:\PrimaryB"), true);
        state.tab_mut(PaneId::Primary).unwrap().selected_paths =
            vec![PathBuf::from(r"C:\PrimaryB\alpha.txt")];
        state.panes[0].scroll_offset = 128.5;

        state
            .tab_mut(PaneId::Secondary)
            .unwrap()
            .navigate(PathBuf::from(r"D:\SecondaryB"), true);
        state.tab_mut(PaneId::Secondary).unwrap().selected_paths =
            vec![PathBuf::from(r"D:\SecondaryB\beta.txt")];
        state.panes[1].scroll_offset = 42.0;

        assert!(state.swap_panes());

        assert_eq!(state.active, PaneId::Secondary);
        assert_eq!(state.path(PaneId::Primary), Path::new(r"D:\SecondaryB"));
        assert_eq!(state.path(PaneId::Secondary), Path::new(r"C:\PrimaryB"));
        assert_eq!(
            state.tab(PaneId::Primary).unwrap().selected_paths,
            vec![PathBuf::from(r"D:\SecondaryB\beta.txt")]
        );
        assert_eq!(
            state.tab(PaneId::Secondary).unwrap().selected_paths,
            vec![PathBuf::from(r"C:\PrimaryB\alpha.txt")]
        );
        assert!(state.tab(PaneId::Primary).unwrap().history.can_go_back());
        assert!(state.tab(PaneId::Secondary).unwrap().history.can_go_back());
        assert_eq!(
            state.tab_mut(PaneId::Primary).unwrap().history.go_back(),
            Some(&PathBuf::from(r"D:\Secondary"))
        );
        assert_eq!(
            state.tab_mut(PaneId::Secondary).unwrap().history.go_back(),
            Some(&PathBuf::from(r"C:\PrimaryA"))
        );
        assert_eq!(state.panes[0].scroll_offset, 42.0);
        assert_eq!(state.panes[1].scroll_offset, 128.5);
    }

    #[test]
    fn move_tab_to_other_pane_no_op_in_single_pane_mode() {
        let mut state = test_state(false);
        state.tabs.new_tab(PathBuf::from(r"C:\PrimaryB"));
        assert!(!state.move_active_tab_to_other_pane());
        assert_eq!(state.tabs.tabs().len(), 2);
        assert_eq!(state.path(PaneId::Secondary), Path::new(r"D:\Secondary"));
    }

    #[test]
    fn move_primary_tab_to_secondary_preserves_history_selection_and_scroll() {
        let mut state = test_state(true);
        state.panes[0].scroll_offset = 15.0;
        let first_id = state.tabs.active_tab().unwrap().id;
        state.tab_scroll_offsets.insert(first_id, 15.0);

        state.tabs.new_tab(PathBuf::from(r"C:\TabTwo"));
        state
            .tabs
            .active_tab_mut()
            .unwrap()
            .navigate(PathBuf::from(r"C:\TabTwo\Sub"), true);
        state.tabs.active_tab_mut().unwrap().selected_paths =
            vec![PathBuf::from(r"C:\TabTwo\Sub\doc.md")];
        state.panes[0].scroll_offset = 240.0;

        assert!(state.move_active_tab_to_other_pane());

        assert_eq!(state.tabs.tabs().len(), 1);
        assert_eq!(state.path(PaneId::Primary), Path::new(r"C:\PrimaryA"));
        assert_eq!(state.panes[0].scroll_offset, 15.0);
        assert_eq!(state.path(PaneId::Secondary), Path::new(r"C:\TabTwo\Sub"));
        assert_eq!(
            state.secondary_tab.selected_paths,
            vec![PathBuf::from(r"C:\TabTwo\Sub\doc.md")]
        );
        assert_eq!(
            state.secondary_tab.history.go_back(),
            Some(&PathBuf::from(r"C:\TabTwo"))
        );
        assert_eq!(state.panes[1].scroll_offset, 240.0);
        assert_eq!(state.active, PaneId::Secondary);
    }

    #[test]
    fn move_secondary_tab_to_primary_preserves_history_selection_and_scroll() {
        let mut state = test_state(true);
        state.active = PaneId::Secondary;
        state
            .secondary_tab
            .navigate(PathBuf::from(r"D:\Secondary\Deep"), true);
        state.secondary_tab.selected_paths = vec![PathBuf::from(r"D:\Secondary\Deep\file.rs")];
        state.panes[1].scroll_offset = 310.0;

        assert!(state.move_active_tab_to_other_pane());

        assert_eq!(state.active, PaneId::Primary);
        assert_eq!(state.tabs.tabs().len(), 2);
        assert_eq!(state.path(PaneId::Primary), Path::new(r"D:\Secondary\Deep"));
        assert_eq!(
            state.tab(PaneId::Primary).unwrap().selected_paths,
            vec![PathBuf::from(r"D:\Secondary\Deep\file.rs")]
        );
        assert_eq!(
            state.tab_mut(PaneId::Primary).unwrap().history.go_back(),
            Some(&PathBuf::from(r"D:\Secondary"))
        );
        assert_eq!(state.panes[0].scroll_offset, 310.0);
    }

    #[test]
    fn move_sole_primary_tab_to_secondary_swaps_cleanly() {
        let mut state = test_state(true);
        state.panes[0].scroll_offset = 77.0;
        state.panes[1].scroll_offset = 12.0;

        assert!(state.move_active_tab_to_other_pane());

        assert_eq!(state.tabs.tabs().len(), 1);
        assert_eq!(state.path(PaneId::Primary), Path::new(r"D:\Secondary"));
        assert_eq!(state.path(PaneId::Secondary), Path::new(r"C:\PrimaryA"));
        assert_eq!(state.panes[0].scroll_offset, 12.0);
        assert_eq!(state.panes[1].scroll_offset, 77.0);
        assert_eq!(state.active, PaneId::Secondary);
    }

    #[test]
    fn state_reset_layout_and_session_round_trip() {
        let mut state = test_state(true);
        state.sidebar_visible = false;
        state.config.show_preview_pane = false;
        state.sidebar_width = 320.0;
        state.preview_width = Some(380.0);
        state.pane_split_ratio = 0.65;
        state.toggle_sidebar_group("favorites");
        state.toggle_preview_side();

        let saved = state.session(1600, 900);
        assert!(!saved.sidebar_visible);
        assert_eq!(saved.sidebar_width, 320.0);
        assert_eq!(saved.preview_width, Some(380.0));
        assert_eq!(saved.pane_split_ratio, 0.65);
        assert_eq!(
            saved.collapsed_sidebar_groups,
            vec!["favorites".to_string()]
        );
        assert_eq!(saved.preview_side, PreviewSide::Left);

        state.reset_layout();
        assert!(state.sidebar_visible);
        assert!(state.config.show_preview_pane);
        assert_eq!(state.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
        assert_eq!(state.preview_width, None);
        assert_eq!(state.pane_split_ratio, DEFAULT_PANE_SPLIT_RATIO);
        assert!(state.collapsed_sidebar_groups.is_empty());
        assert_eq!(state.preview_side, PreviewSide::Right);
        assert_eq!(state.path(PaneId::Primary), Path::new(r"C:\PrimaryA"));
        assert_eq!(state.path(PaneId::Secondary), Path::new(r"D:\Secondary"));
    }
}
