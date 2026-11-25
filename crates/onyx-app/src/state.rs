use onyx_cache::{DirectoryCache, IconCache};
use onyx_config::{AppConfig, ConfigStore, SessionState};
use onyx_core::{FileEntry, GenerationId, PaneId, TabManager, TabState, ThemeMode};
use onyx_operations::FileOperationsEngine;
use onyx_preview::ThumbnailEngine;
use onyx_search::{SearchIndex, UnifiedSearchEngine};
use onyx_ui::UiFileEntry;
use onyx_windows::DropTargetGuard;
use slint::VecModel;
use std::collections::{HashSet, VecDeque};
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
    pub entries: Vec<FileEntry>,
    pub model: Rc<VecModel<UiFileEntry>>,
    pub loading: bool,
    pub error: String,
    pub query: String,
    pub result_order: bool,
}

impl Pane {
    pub fn restart(&mut self) -> (GenerationId, CancellationToken) {
        self.cancel.cancel();
        self.thumb_cancel.cancel();
        self.generation = self.generation.next();
        self.cancel = CancellationToken::new();
        self.thumb_cancel = self.cancel.child_token();
        self.loading = true;
        self.error.clear();
        (self.generation, self.cancel.clone())
    }

    pub fn restart_thumbnails(&mut self) -> (GenerationId, CancellationToken) {
        self.thumb_cancel.cancel();
        self.thumb_cancel = self.cancel.child_token();
        (self.generation, self.thumb_cancel.clone())
    }
}

pub(crate) struct State {
    pub store: ConfigStore,
    pub config: AppConfig,
    pub tabs: TabManager,
    pub secondary_tab: TabState,
    pub panes: [Pane; 2],
    pub active: PaneId,
    pub home: PathBuf,
    pub dual: bool,
    pub quick_look: bool,
    pub directories: DirectoryCache,
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
        session: SessionState,
        home: PathBuf,
        path: Option<PathBuf>,
    ) -> Self {
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
            active: PaneId::Primary,
            home,
            dual,
            quick_look: false,
            directories: DirectoryCache::default(),
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

        // Restarting the pane increments generation and cancels both directory and thumbnail tokens.
        let (g2, c2) = pane.restart();
        assert_eq!(g2, GenerationId::new(2));
        assert!(c1.is_cancelled());
        assert!(tc2.is_cancelled());
        assert!(!c2.is_cancelled());
    }

    #[test]
    fn zip_extension_detection_is_case_insensitive() {
        assert!(is_zip_path(Path::new(r"C:\archive.zip")));
        assert!(is_zip_path(Path::new(r"C:\ARCHIVE.ZIP")));
        assert!(!is_zip_path(Path::new(r"C:\archive.tar.gz")));
        assert!(!is_zip_path(Path::new(r"C:\zip")));
    }
}
