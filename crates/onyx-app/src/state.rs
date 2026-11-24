use onyx_cache::{DirectoryCache, IconCache};
use onyx_config::{AppConfig, ConfigStore, SessionState};
use onyx_core::{FileEntry, GenerationId, PaneId, TabManager, TabState, ThemeMode};
use onyx_operations::FileOperationsEngine;
use onyx_search::UnifiedSearchEngine;
use onyx_ui::UiFileEntry;
use slint::VecModel;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::operations::Prompt;

#[derive(Default)]
pub(crate) struct Pane {
    pub generation: GenerationId,
    pub cancel: CancellationToken,
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
        self.generation = self.generation.next();
        self.cancel = CancellationToken::new();
        self.loading = true;
        self.error.clear();
        (self.generation, self.cancel.clone())
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
    pub preview_cancel: CancellationToken,
    pub preview_path: Option<PathBuf>,
    pub search: UnifiedSearchEngine,
    pub index_path: PathBuf,
    pub indexed_search: bool,
    pub indexed_roots: HashSet<PathBuf>,
    pub indexing: Option<(PathBuf, CancellationToken)>,
    pub operations: Arc<FileOperationsEngine>,
    pub operation: Option<(CancellationToken, JoinHandle<()>)>,
    pub prompt: Option<Prompt>,
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
            preview_cancel: CancellationToken::new(),
            preview_path: None,
            search: UnifiedSearchEngine::live_only(),
            index_path,
            indexed_search: false,
            indexed_roots: HashSet::new(),
            indexing: None,
            operations: Arc::new(FileOperationsEngine::new()),
            operation: None,
            prompt: None,
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

    pub fn visible_panes(&self) -> impl Iterator<Item = PaneId> + use<> {
        [PaneId::Primary, PaneId::Secondary]
            .into_iter()
            .take(if self.dual { 2 } else { 1 })
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
