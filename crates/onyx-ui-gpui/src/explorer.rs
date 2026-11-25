//! Primary Onyx GPUI explorer coordinator wired to `onyx-core`, `onyx-fs`,
//! `onyx-cache`, `onyx-config`, and `onyx-windows`.

use crate::components::archive_overlay::{self, ArchiveViewState};
use crate::components::command_palette::{
    self, CommandPaletteState, PaletteMode, file_search_query, palette_mode,
};
use crate::components::context_menu::{ContextMenuAction, ContextMenuState, ContextMenuTarget};
use crate::components::drag_drop::{self, DragState, DropTarget, validate_drop};
use crate::components::preview_pane::{DecodedPreview, PreviewKind, PreviewState};
use crate::components::settings_overlay;
use crate::components::smooth_scroll::{
    DETAILS_ROW_HEIGHT, SmoothScroll, content_height, keep_visible, wheel_pixels,
};
use crate::components::splitter::{self, ActiveSplitterDrag, SplitterKind};
use crate::components::{
    context_menu, file_pane, nav_bar, overlays, preview_pane, sidebar, status_bar, tab_bar, toolbar,
};
use crate::icons::{ShellIconStore, entry_icon_key, path_icon_key, rgba_pixels_to_render_image};
use crate::search_index::{
    self, IndexController, IndexEvent, IndexStatus, ListMode, SearchRun, SearchStatus,
};
use crate::theme::OnyxTheme;
use gpui_kit::{
    AppContext, ClipboardItem, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    Point, Render, RenderImage, ScrollEvent, ScrollHandle, Styled, Subscription,
    UniformListScrollHandle, Window,
    component::input::{InputEvent, InputState},
    div, point, px,
};
use onyx_cache::{
    CachedFolderSize, DirectoryCache, FolderSizeCache, FolderSizeStore, dir_mtime_secs,
};
use onyx_config::{AppConfig, ConfigStore, SessionState};
use onyx_core::{
    DriveInfo, FavoriteItem, FileEntry, GenerationId, KnownFolder, NavHistory, OnyxError,
    OnyxResult, SortColumn, SortDirection, SortSpec, TabManager, ThemeMode, ViewMode,
    expand_env_vars, format_bytes, natural_cmp, normalize_display_path, sort_entries,
    split_autocomplete_prefix,
};
use onyx_fs::{
    DEFAULT_MAX_FOLDER_SIZE_ENTRIES, DirectoryWatcher, EnumerationOptions, FolderSizeResult,
    FsEngine, WatchEvent, compute_folder_size,
};
use onyx_operations::{
    ConflictPolicy, FileOperationsEngine, OperationProgress, OperationReport,
    PermanentDeleteRequest, UndoAction, create_archive, extract_archive, is_supported_archive_path,
    list_archive_entries,
};
use onyx_preview::{PreviewEngine, ThumbnailEngine, is_thumbnail_supported};
use onyx_search::{SearchIndex, SearchQuery, SearchResult, UnifiedSearchEngine};
use onyx_windows::{
    RgbaIcon, TerminalProfile, enumerate_drives, enumerate_known_folders, extract_file_icon_rgba,
    extract_path_icon_rgba, get_clipboard_files, launch_terminal_with_profile, open_file,
    open_with_dialog, set_clipboard_files, show_context_menu, show_properties_dialog,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const MAX_FOLDER_SIZE_CONCURRENCY: usize = 2;
const THUMBNAIL_TARGET_PX: u32 = 128;
const MAX_THUMBNAIL_WORKERS: usize = 4;

type FileList = (Vec<PathBuf>, bool);

/// Kind of modal prompt currently open.
#[derive(Debug, Clone)]
pub(crate) enum PromptKind {
    NewFolder(PathBuf),
    Rename(PathBuf),
    PermanentDelete(PathBuf),
}

/// Active state for the reusable Prompt Dialog modal.
#[derive(Debug, Clone)]
pub(crate) struct PromptState {
    pub kind: PromptKind,
    pub title: String,
    pub message: String,
    pub confirm_label: String,
    pub is_danger: bool,
    pub show_input: bool,
}

/// Live state for the bottom-right floating Operation Progress toast.
#[derive(Debug, Clone)]
pub(crate) struct ActiveOperation {
    pub title: &'static str,
    pub current_item: String,
    pub progress: f32,
    pub detail: String,
    pub cancel: CancellationToken,
}

enum ClipboardCommand {
    Write(FileList),
    Read(oneshot::Sender<Option<FileList>>),
    Moved {
        original: Vec<PathBuf>,
        moved: Vec<PathBuf>,
    },
}

enum ClipboardUpdate {
    CanPaste(bool),
    Status(String),
}

#[derive(Default)]
struct ClipboardState {
    observed: Option<FileList>,
    fallback: Option<FileList>,
    consumed: Vec<PathBuf>,
}

impl ClipboardState {
    fn read(&mut self) -> Option<FileList> {
        let system = get_clipboard_files();
        if system.is_some() && system != self.observed {
            self.observed = system.clone();
            self.fallback = None;
            self.consumed.clear();
        } else if system.is_none() && self.fallback.is_none() {
            self.observed = None;
            self.consumed.clear();
        }
        let (mut paths, cut) = self.fallback.clone().or(system)?;
        if cut {
            paths.retain(|path| !self.consumed.iter().any(|moved| same_path(moved, path)));
        }
        (!paths.is_empty()).then_some((paths, cut))
    }

    fn write(&mut self, files: FileList) -> Option<String> {
        let (paths, cut) = &files;
        self.consumed.clear();
        match set_clipboard_files(paths, *cut) {
            Ok(()) => {
                self.fallback = None;
                self.observed = Some(files);
                None
            }
            Err(error) => {
                self.observed = get_clipboard_files();
                self.fallback = Some(files);
                Some(format!("Available in Onyx only · {error}"))
            }
        }
    }
}

enum FileOperation {
    Create {
        parent: PathBuf,
        name: String,
    },
    Rename {
        source: PathBuf,
        name: String,
    },
    Duplicate(PathBuf),
    Paste(PathBuf),
    Recycle(PathBuf),
    PermanentDelete(PermanentDeleteRequest),
    CopyItems {
        sources: Vec<PathBuf>,
        destination: PathBuf,
    },
    MoveItems {
        sources: Vec<PathBuf>,
        destination: PathBuf,
    },
    CreateArchive {
        sources: Vec<PathBuf>,
        current_dir: PathBuf,
    },
    ExtractArchive {
        archive_path: PathBuf,
        dest_dir: PathBuf,
    },
    Undo,
}

impl FileOperation {
    const fn title(&self) -> &'static str {
        match self {
            Self::Create { .. } => "New folder",
            Self::Rename { .. } => "Rename",
            Self::Duplicate(_) => "Duplicate",
            Self::Paste(_) => "Paste",
            Self::Recycle(_) => "Move to Recycle Bin",
            Self::PermanentDelete(_) => "Permanent delete",
            Self::CopyItems { .. } => "Copy",
            Self::MoveItems { .. } => "Move",
            Self::CreateArchive { .. } => "Compress to ZIP",
            Self::ExtractArchive { .. } => "Extract ZIP",
            Self::Undo => "Undo",
        }
    }

    async fn run(
        self,
        engine: Arc<FileOperationsEngine>,
        clipboard: mpsc::UnboundedSender<ClipboardCommand>,
        cancel: CancellationToken,
        progress: mpsc::Sender<OperationProgress>,
        affected: &mut Vec<PathBuf>,
    ) -> OnyxResult<Option<OperationReport>> {
        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }
        let report = match self {
            Self::Create { parent, name } => {
                affected.push(parent.clone());
                engine.create_dir(&parent, &name).await
            }
            Self::Rename { source, name } => {
                add_parent(affected, &source);
                engine
                    .rename_item(&source, &name, ConflictPolicy::Fail)
                    .await
            }
            Self::Duplicate(source) => {
                add_parent(affected, &source);
                engine.duplicate_item(&source, cancel, Some(progress)).await
            }
            Self::Paste(destination) => {
                let (reply, answer) = oneshot::channel();
                let _ = clipboard.send(ClipboardCommand::Read(reply));
                let files = tokio::select! {
                    _ = cancel.cancelled() => return Err(OnyxError::Cancelled),
                    answer = answer => answer.ok().flatten(),
                };
                let Some((paths, cut)) = files else {
                    return Err(OnyxError::OperationFailed {
                        message: "The clipboard has no files to paste.".into(),
                    });
                };
                affected.push(destination.clone());
                if cut {
                    for path in &paths {
                        add_parent(affected, path);
                    }
                    let report = engine
                        .move_items(
                            &paths,
                            &destination,
                            ConflictPolicy::AutoRename,
                            cancel,
                            Some(progress),
                        )
                        .await;
                    if let Ok(report) = &report
                        && let Some(UndoAction::ReverseMoves(moves)) = &report.undo_action
                    {
                        let moved = moves.iter().map(|(_, source)| source.clone()).collect();
                        let _ = clipboard.send(ClipboardCommand::Moved {
                            original: paths,
                            moved,
                        });
                    }
                    report
                } else {
                    engine
                        .copy_items(
                            &paths,
                            &destination,
                            ConflictPolicy::AutoRename,
                            cancel,
                            Some(progress),
                        )
                        .await
                }
            }
            Self::Recycle(path) => {
                add_parent(affected, &path);
                engine.recycle_items(&[path], cancel, Some(progress)).await
            }
            Self::PermanentDelete(request) => {
                for path in &request.paths {
                    add_parent(affected, path);
                }
                engine
                    .permanent_delete(request, cancel, Some(progress))
                    .await
            }
            Self::CopyItems {
                sources,
                destination,
            } => {
                affected.push(destination.clone());
                for path in &sources {
                    add_parent(affected, path);
                }
                engine
                    .copy_items(
                        &sources,
                        &destination,
                        ConflictPolicy::AutoRename,
                        cancel,
                        Some(progress),
                    )
                    .await
            }
            Self::MoveItems {
                sources,
                destination,
            } => {
                affected.push(destination.clone());
                for path in &sources {
                    add_parent(affected, path);
                }
                engine
                    .move_items(
                        &sources,
                        &destination,
                        ConflictPolicy::AutoRename,
                        cancel,
                        Some(progress),
                    )
                    .await
            }
            Self::CreateArchive {
                sources,
                current_dir,
            } => {
                affected.push(current_dir.clone());
                let sources_for_dest = sources.clone();
                let cancel_for_dest = cancel.clone();
                let dest_zip = tokio::task::spawn_blocking(move || {
                    if cancel_for_dest.is_cancelled() {
                        return Err(OnyxError::Cancelled);
                    }
                    Ok(archive_destination(
                        &sources_for_dest,
                        &current_dir,
                        |path| path.exists(),
                    ))
                })
                .await
                .map_err(|err| OnyxError::OperationFailed {
                    message: format!("Archive destination task join failed: {err}"),
                })??;
                add_parent(affected, &dest_zip);
                create_archive(sources, dest_zip, Some(progress), cancel).await
            }
            Self::ExtractArchive {
                archive_path,
                dest_dir,
            } => {
                affected.push(dest_dir.clone());
                add_parent(affected, &dest_dir);
                extract_archive(
                    archive_path,
                    dest_dir,
                    ConflictPolicy::AutoRename,
                    Some(progress),
                    cancel,
                )
                .await
            }
            Self::Undo => return engine.undo_last().await,
        };
        report.map(Some)
    }
}

enum OperationEvent {
    Progress(OperationProgress),
    Finished {
        result: OnyxResult<Option<OperationReport>>,
        can_undo: bool,
        affected: Vec<PathBuf>,
        is_undo: bool,
    },
}

/// Identifies one of the two file panes in dual-pane mode (Stage 2D).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PaneId {
    #[default]
    Primary,
    Secondary,
}

/// State of the secondary pane. The primary pane keeps the flat fields on
/// [`OnyxExplorer`] so single-pane rendering stays untouched.
struct SecondaryPane {
    path: PathBuf,
    all_entries: Vec<FileEntry>,
    entries: Vec<FileEntry>,
    selected_index: Option<usize>,
    is_loading: bool,
    error_message: Option<String>,
    sort_spec: SortSpec,
    list_scroll_handle: ScrollHandle,
    list_scroll: SmoothScroll,
    grid_scroll_handle: UniformListScrollHandle,
    generation: GenerationId,
    cancel_token: CancellationToken,
    folder_size_cancel: CancellationToken,
    active_folder_size_tasks: usize,
    watched_path: Option<PathBuf>,
}

impl SecondaryPane {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            all_entries: Vec::new(),
            entries: Vec::new(),
            selected_index: None,
            is_loading: false,
            error_message: None,
            sort_spec: SortSpec::default(),
            list_scroll_handle: ScrollHandle::new(),
            list_scroll: SmoothScroll::new(),
            grid_scroll_handle: UniformListScrollHandle::new(),
            generation: GenerationId::new(0),
            cancel_token: CancellationToken::new(),
            folder_size_cancel: CancellationToken::new(),
            active_folder_size_tasks: 0,
            watched_path: None,
        }
    }
}

/// Borrowed snapshot of one pane's visible list, used by the status bar.
#[derive(Debug, Clone, Copy)]
pub struct PaneSnapshot<'a> {
    pub entries: &'a [FileEntry],
    pub selected_index: Option<usize>,
}

/// Main GPUI view for Onyx Stage 1 (visual parity with Slint UI).
pub struct OnyxExplorer {
    pub(crate) runtime: Arc<Runtime>,
    pub(crate) theme: OnyxTheme,
    pub(crate) focus_handle: FocusHandle,
    pub(crate) tabs: TabManager,
    pub(crate) home_path: PathBuf,
    pub(crate) favorites: Vec<FavoriteItem>,
    pub(crate) known_folders: Vec<KnownFolder>,
    pub(crate) drives: Vec<DriveInfo>,
    pub(crate) collapsed_sidebar_groups: HashSet<String>,
    pub(crate) sidebar_visible: bool,
    pub(crate) preview_visible: bool,
    pub(crate) quick_look_open: bool,
    pub(crate) preview: PreviewState,
    pub(crate) preview_generation: GenerationId,
    pub(crate) preview_cancel: CancellationToken,
    pub(crate) view_mode: ViewMode,
    pub(crate) show_hidden: bool,
    pub(crate) calculate_folder_sizes: bool,
    pub(crate) sort_spec: SortSpec,
    pub(crate) all_entries: Vec<FileEntry>,
    pub(crate) entries: Vec<FileEntry>,
    pub(crate) selected_index: Option<usize>,
    pub(crate) generation: GenerationId,
    pub(crate) cancel_token: CancellationToken,
    pub(crate) folder_size_cancel: CancellationToken,
    pub(crate) active_folder_size_tasks: usize,
    pub(crate) thumbnail_engine: ThumbnailEngine,
    pub(crate) thumbnails: HashMap<PathBuf, Arc<RenderImage>>,
    pub(crate) thumbnail_generation: GenerationId,
    pub(crate) thumbnail_cancel: CancellationToken,
    pub(crate) is_loading: bool,
    pub(crate) error_message: Option<String>,
    pub(crate) anim_frame: usize,
    pub(crate) anim_running: bool,
    pub(crate) directories: DirectoryCache,
    pub(crate) folder_sizes: FolderSizeCache,
    pub(crate) folder_size_store: Option<Arc<FolderSizeStore>>,
    pub(crate) icons: ShellIconStore,
    pub(crate) address_input: Entity<InputState>,
    pub(crate) address_editing: bool,
    pub(crate) address_generation: u64,
    pub(crate) address_suggestions: Vec<String>,
    pub(crate) suggestion_index: Option<usize>,
    pub(crate) search_input: Entity<InputState>,
    pub(crate) search_query: String,
    pub(crate) indexed_search: bool,
    /// Which source feeds the primary pane's list.
    pub(crate) list_mode: ListMode,
    /// Generation and cancellation of the list search run in flight.
    pub(crate) search_run: SearchRun,
    /// Hits of the current list search run, in rank order.
    pub(crate) search_hits: Vec<FileEntry>,
    /// Preferences for the library search: indexed when an index is open,
    /// otherwise a live walk of the current folder tree.
    pub(crate) search_engine: UnifiedSearchEngine,
    /// Shared handle of the open on-disk index, handed over by the worker.
    pub(crate) search_index: Option<Arc<Mutex<SearchIndex>>>,
    pub(crate) index: IndexController,
    pub(crate) index_status: IndexStatus,
    /// Generation and cancellation of the running `index_directory` build.
    pub(crate) build_generation: GenerationId,
    pub(crate) build_cancel: CancellationToken,
    /// Root of a build that waits for the worker to finish opening the index.
    rebuild_pending: Option<PathBuf>,
    /// Hit selected by the command palette, revealed once its folder loaded.
    pending_selection: Option<PathBuf>,
    pub(crate) list_scroll_handle: ScrollHandle,
    pub(crate) list_scroll: SmoothScroll,
    /// Set while a frame callback is armed for the scroll glide; render re-arms
    /// it each frame so the easing runs on the display clock.
    pub(crate) scroll_frame_armed: bool,
    pub(crate) grid_scroll_handle: UniformListScrollHandle,
    pub(crate) operations: Arc<FileOperationsEngine>,
    clipboard_tx: mpsc::UnboundedSender<ClipboardCommand>,
    shutdown_token: CancellationToken,
    pub(crate) can_paste: bool,
    pub(crate) can_undo: bool,
    pub(crate) status_message: Option<String>,
    pub(crate) prompt_state: Option<PromptState>,
    pub(crate) prompt_input: Entity<InputState>,
    pub(crate) palette_input: Entity<InputState>,
    pub(crate) prompt_focus_handle: FocusHandle,
    pub(crate) active_operation: Option<ActiveOperation>,
    pub(crate) context_menu: Option<ContextMenuState>,
    pub(crate) dual_pane: bool,
    pub(crate) active_pane: PaneId,
    secondary: SecondaryPane,
    pub(crate) sidebar_width: f32,
    pub(crate) preview_width: Option<f32>,
    pub(crate) pane_split_ratio: f32,
    pub(crate) splitter_drag: Option<ActiveSplitterDrag>,
    pub(crate) command_palette: Option<CommandPaletteState>,
    pub(crate) settings_open: bool,
    pub(crate) archive: Option<ArchiveViewState>,
    archive_generation: GenerationId,
    archive_cancel: CancellationToken,
    pub(crate) drag: Option<DragState>,
    /// Last painted viewport size, used for layout clamping and session persistence.
    pub(crate) viewport_size: (f32, f32),
    watch_cmd_tx: mpsc::UnboundedSender<PathBuf>,
    watched_path: Option<PathBuf>,
    watch_cmd_tx_secondary: mpsc::UnboundedSender<PathBuf>,
    session_write_gate: Arc<Mutex<()>>,
    layout_touched: bool,
    config_store: ConfigStore,
    pub(crate) config: AppConfig,
    _subscriptions: Vec<Subscription>,
}

impl Drop for OnyxExplorer {
    fn drop(&mut self) {
        self.shutdown_token.cancel();
    }
}

impl std::fmt::Debug for OnyxExplorer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnyxExplorer")
            .field("current_path", &self.current_path())
            .field("entries", &self.entries.len())
            .field("dark_mode", &self.theme.dark_mode)
            .finish()
    }
}

impl OnyxExplorer {
    /// Creates the explorer state, binds inputs, and starts initial async loading.
    pub fn new(
        runtime: Arc<Runtime>,
        initial_path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let theme = OnyxTheme::default();
        theme.sync_gpui_theme(Some(window), cx);

        let focus_handle = cx.focus_handle();
        let prompt_focus_handle = cx.focus_handle();
        let home_path = dirs::home_dir().unwrap_or_else(|| initial_path.clone());
        let address_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Enter path and press Enter..."));
        let search_input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter folder..."));
        let prompt_input = cx.new(|cx| InputState::new(window, cx));
        let palette_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Type a command, or ? to search files…")
        });

        let addr_sub = cx.subscribe_in(
            &address_input,
            window,
            |explorer, state, ev: &InputEvent, window, cx| match ev {
                InputEvent::Change => {
                    let text = state.read(cx).value().to_string();
                    explorer.on_address_changed(&text, cx);
                }
                InputEvent::PressEnter { .. } => {
                    let text = state.read(cx).value().to_string();
                    explorer.submit_address(&text, window, cx);
                }
                InputEvent::Blur => {
                    if explorer.address_editing {
                        explorer.address_editing = false;
                        explorer.address_suggestions.clear();
                        explorer.suggestion_index = None;
                        cx.notify();
                    }
                }
                InputEvent::Focus => {}
            },
        );

        let search_sub = cx.subscribe_in(
            &search_input,
            window,
            |explorer, state, ev: &InputEvent, _window, cx| match ev {
                InputEvent::Change | InputEvent::PressEnter { .. } => {
                    let query = state.read(cx).value().to_string();
                    explorer.on_search_changed(query, cx);
                }
                InputEvent::Focus | InputEvent::Blur => {}
            },
        );

        let prompt_sub = cx.subscribe_in(
            &prompt_input,
            window,
            |explorer, _state, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) && explorer.prompt_state.is_some() {
                    explorer.confirm_prompt(window, cx);
                }
            },
        );

        let palette_sub = cx.subscribe_in(
            &palette_input,
            window,
            |explorer, state, ev: &InputEvent, window, cx| match ev {
                InputEvent::Change => {
                    let query = state.read(cx).value().to_string();
                    explorer.on_palette_query_changed(query, cx);
                }
                InputEvent::PressEnter { .. } => {
                    explorer.activate_palette_selection(window, cx);
                }
                InputEvent::Focus | InputEvent::Blur => {}
            },
        );

        let (clipboard_tx, clipboard_rx) = mpsc::unbounded_channel();
        let shutdown_token = CancellationToken::new();
        let watch_cmd_tx = spawn_directory_watcher(
            Arc::clone(&runtime),
            shutdown_token.clone(),
            PaneId::Primary,
            cx,
        );
        let watch_cmd_tx_secondary = spawn_directory_watcher(
            Arc::clone(&runtime),
            shutdown_token.clone(),
            PaneId::Secondary,
            cx,
        );
        let config_store = ConfigStore::default_location();
        // The index worker owns the database and the watcher; the explorer only
        // sends commands and receives `IndexEvent`s on the GPUI thread.
        let (index_events_tx, index_events_rx) = mpsc::unbounded_channel();
        let index = IndexController::start(
            crate::search_index::index_path(config_store.dir()),
            &shutdown_token,
            index_events_tx,
        );
        let mut explorer = Self {
            runtime,
            theme,
            focus_handle,
            tabs: TabManager::new(initial_path.clone()),
            home_path,
            favorites: Vec::new(),
            known_folders: Vec::new(),
            drives: Vec::new(),
            collapsed_sidebar_groups: HashSet::new(),
            sidebar_visible: true,
            preview_visible: false,
            quick_look_open: false,
            preview: PreviewState::default(),
            preview_generation: GenerationId::new(0),
            preview_cancel: CancellationToken::new(),
            view_mode: ViewMode::Details,
            show_hidden: false,
            calculate_folder_sizes: true,
            sort_spec: SortSpec::default(),
            all_entries: Vec::new(),
            entries: Vec::new(),
            selected_index: None,
            generation: GenerationId::new(0),
            cancel_token: CancellationToken::new(),
            folder_size_cancel: CancellationToken::new(),
            active_folder_size_tasks: 0,
            thumbnail_engine: ThumbnailEngine::new(),
            thumbnails: HashMap::new(),
            thumbnail_generation: GenerationId::new(0),
            thumbnail_cancel: CancellationToken::new(),
            is_loading: false,
            error_message: None,
            anim_frame: 0,
            anim_running: false,
            directories: DirectoryCache::default(),
            folder_sizes: FolderSizeCache::default(),
            folder_size_store: None,
            icons: ShellIconStore::default(),
            address_input,
            address_editing: false,
            address_generation: 0,
            address_suggestions: Vec::new(),
            suggestion_index: None,
            search_input,
            search_query: String::new(),
            indexed_search: false,
            list_mode: ListMode::Browse,
            search_run: SearchRun::default(),
            search_hits: Vec::new(),
            search_engine: UnifiedSearchEngine::live_only(),
            search_index: None,
            index,
            index_status: IndexStatus::Disabled,
            build_generation: GenerationId::new(0),
            build_cancel: CancellationToken::new(),
            rebuild_pending: None,
            pending_selection: None,
            list_scroll_handle: ScrollHandle::new(),
            list_scroll: SmoothScroll::new(),
            scroll_frame_armed: false,
            grid_scroll_handle: UniformListScrollHandle::new(),
            operations: Arc::new(FileOperationsEngine::new()),
            clipboard_tx,
            shutdown_token,
            can_paste: false,
            can_undo: false,
            status_message: None,
            prompt_state: None,
            prompt_input,
            palette_input,
            prompt_focus_handle,
            active_operation: None,
            context_menu: None,
            dual_pane: false,
            active_pane: PaneId::Primary,
            secondary: SecondaryPane::new(initial_path.clone()),
            sidebar_width: onyx_config::DEFAULT_SIDEBAR_WIDTH,
            preview_width: None,
            pane_split_ratio: onyx_config::DEFAULT_PANE_SPLIT_RATIO,
            splitter_drag: None,
            command_palette: None,
            settings_open: false,
            archive: None,
            archive_generation: GenerationId::new(0),
            archive_cancel: CancellationToken::new(),
            drag: None,
            viewport_size: (1320.0, 820.0),
            watch_cmd_tx,
            watched_path: None,
            watch_cmd_tx_secondary,
            session_write_gate: Arc::new(Mutex::new(())),
            layout_touched: false,
            config_store,
            config: AppConfig::default(),
            _subscriptions: vec![addr_sub, search_sub, prompt_sub, palette_sub],
        };

        explorer.start_clipboard_monitor(clipboard_rx, cx);
        explorer.start_index_event_loop(index_events_rx, cx);
        explorer.spawn_bootstrap_sidebar_and_config(cx);
        explorer.load_directory(initial_path, false, true, cx);
        explorer
    }

    // ---- Queries for modular components ----

    #[must_use]
    pub fn current_path(&self) -> &Path {
        self.tabs
            .active_tab()
            .map_or(&self.home_path, |tab| &tab.path)
    }

    #[must_use]
    pub fn active_history(&self) -> Option<&NavHistory> {
        self.tabs.active_tab().map(|tab| &tab.history)
    }

    #[must_use]
    pub fn is_current_favorite(&self) -> bool {
        let current = self.current_path();
        self.favorites
            .iter()
            .any(|fav| same_path(&fav.path, current))
    }

    #[must_use]
    pub fn is_sidebar_group_collapsed(&self, group_id: &str) -> bool {
        self.collapsed_sidebar_groups.contains(group_id)
    }

    #[must_use]
    pub fn search_query_text(&self) -> &str {
        &self.search_query
    }

    #[must_use]
    pub fn has_selection(&self) -> bool {
        self.selected_entry().is_some()
    }

    #[must_use]
    pub fn selected_entry(&self) -> Option<&FileEntry> {
        self.pane_selected_index(self.active_pane)
            .and_then(|idx| self.pane_entries(self.active_pane).get(idx))
    }

    #[must_use]
    pub fn can_paste(&self) -> bool {
        self.can_paste
    }

    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.can_undo
    }

    #[must_use]
    pub fn is_activity_active(&self) -> bool {
        self.is_loading
            || self.active_folder_size_tasks > 0
            || self.secondary.is_loading
            || self.secondary.active_folder_size_tasks > 0
            || self.active_operation.is_some()
            || self.search_run.is_busy()
            || self.index_status.is_busy()
            || self
                .command_palette
                .as_ref()
                .is_some_and(|palette| palette.search.run.is_busy())
    }

    #[must_use]
    pub fn led_label(&self) -> String {
        if self.active_operation.is_some() {
            "Working".to_string()
        } else if self.search_run.is_busy() {
            "Searching".to_string()
        } else if self.index_status.is_busy() {
            "Indexing".to_string()
        } else if self.is_loading || self.secondary.is_loading {
            "Scanning".to_string()
        } else if self.active_folder_size_tasks > 0 || self.secondary.active_folder_size_tasks > 0 {
            "Sizing".to_string()
        } else if self.active_pane_error().is_some() {
            "Error".to_string()
        } else {
            format!("{} items", self.pane_entries(self.active_pane).len())
        }
    }

    #[must_use]
    fn active_pane_error(&self) -> Option<&str> {
        self.pane_error_message(self.active_pane)
    }

    #[must_use]
    fn pane_max_entry_bytes(&self, pane: PaneId) -> u64 {
        self.pane_entries(pane)
            .iter()
            .filter_map(|entry| {
                if entry.is_dir() {
                    self.folder_sizes.get(&entry.path).map(|s| s.total_bytes)
                } else {
                    Some(entry.size)
                }
            })
            .max()
            .unwrap_or(0)
    }

    #[must_use]
    pub fn entry_size_info_for(&self, pane: PaneId, entry: &FileEntry) -> (String, f32) {
        let max_bytes = self.pane_max_entry_bytes(pane);
        if entry.is_dir() {
            match self.folder_sizes.get(&entry.path) {
                Some(cached) => {
                    let label = if cached.truncated {
                        format!("{}+", format_bytes(cached.total_bytes))
                    } else {
                        format_bytes(cached.total_bytes)
                    };
                    (
                        label,
                        compute_size_ratio(Some(cached.total_bytes), max_bytes),
                    )
                }
                None => (String::new(), 0.0),
            }
        } else {
            (
                format_bytes(entry.size),
                compute_size_ratio(Some(entry.size), max_bytes),
            )
        }
    }

    // ---- Dual-pane queries, focus & layout (Stage 2D) ----

    #[must_use]
    pub fn pane_entries(&self, pane: PaneId) -> &[FileEntry] {
        match pane {
            PaneId::Primary => &self.entries,
            PaneId::Secondary => &self.secondary.entries,
        }
    }

    #[must_use]
    pub fn pane_path(&self, pane: PaneId) -> &Path {
        match pane {
            PaneId::Primary => self.current_path(),
            PaneId::Secondary => &self.secondary.path,
        }
    }

    #[must_use]
    pub fn pane_selected_index(&self, pane: PaneId) -> Option<usize> {
        match pane {
            PaneId::Primary => self.selected_index,
            PaneId::Secondary => self.secondary.selected_index,
        }
    }

    #[must_use]
    pub fn pane_is_loading(&self, pane: PaneId) -> bool {
        match pane {
            PaneId::Primary => self.is_loading,
            PaneId::Secondary => self.secondary.is_loading,
        }
    }

    #[must_use]
    pub fn pane_error_message(&self, pane: PaneId) -> Option<&str> {
        match pane {
            PaneId::Primary => self.error_message.as_deref(),
            PaneId::Secondary => self.secondary.error_message.as_deref(),
        }
    }

    #[must_use]
    pub fn pane_sort_spec(&self, pane: PaneId) -> SortSpec {
        match pane {
            PaneId::Primary => self.sort_spec,
            PaneId::Secondary => self.secondary.sort_spec,
        }
    }

    #[must_use]
    pub fn pane_grid_scroll(&self, pane: PaneId) -> &UniformListScrollHandle {
        match pane {
            PaneId::Primary => &self.grid_scroll_handle,
            PaneId::Secondary => &self.secondary.grid_scroll_handle,
        }
    }

    #[must_use]
    pub fn pane_list_scroll(&self, pane: PaneId) -> &ScrollHandle {
        match pane {
            PaneId::Primary => &self.list_scroll_handle,
            PaneId::Secondary => &self.secondary.list_scroll_handle,
        }
    }

    fn pane_list_smooth(&mut self, pane: PaneId) -> &mut SmoothScroll {
        match pane {
            PaneId::Primary => &mut self.list_scroll,
            PaneId::Secondary => &mut self.secondary.list_scroll,
        }
    }

    /// Scrollable range of the details list for `pane`.
    ///
    /// The tracked handle only ever measures the virtualized window that is
    /// currently rendered, so its `max_offset` is the range of that window (about
    /// `offset + two rows`), not of the folder. A fast wheel spin would be
    /// clamped away against it, so derive the range from the row count instead
    /// and fall back to the handle before the viewport has been measured.
    fn list_scroll_max(&self, pane: PaneId) -> f32 {
        let viewport = self.list_viewport_height(pane);
        if viewport <= 0.0 {
            return f32::from(self.pane_list_scroll(pane).max_offset().y).abs();
        }
        (content_height(self.pane_entries(pane).len(), DETAILS_ROW_HEIGHT) - viewport).max(0.0)
    }

    fn list_viewport_height(&self, pane: PaneId) -> f32 {
        f32::from(self.pane_list_scroll(pane).bounds().size.height).max(0.0)
    }

    // ---- Smooth scrolling of the details list ----

    /// Current eased offset of the details list, in pixels from the top.
    #[must_use]
    pub fn pane_list_offset(&self, pane: PaneId) -> f32 {
        match pane {
            PaneId::Primary => self.list_scroll.offset(),
            PaneId::Secondary => self.secondary.list_scroll.offset(),
        }
    }

    /// Viewport height of the details list measured on the previous frame.
    #[must_use]
    pub fn pane_list_viewport(&self, pane: PaneId) -> f32 {
        self.list_viewport_height(pane)
    }

    fn pane_list_smooth_ref(&self, pane: PaneId) -> &SmoothScroll {
        match pane {
            PaneId::Primary => &self.list_scroll,
            PaneId::Secondary => &self.secondary.list_scroll,
        }
    }

    /// Whether either pane's offset is still easing toward its target.
    fn list_scroll_animating(&self) -> bool {
        let now = Instant::now();
        self.pane_list_smooth_ref(PaneId::Primary).is_animating(now)
            || self
                .pane_list_smooth_ref(PaneId::Secondary)
                .is_animating(now)
    }

    /// Wheel input over the details list: eases the offset instead of jumping.
    pub fn wheel_list(
        &mut self,
        pane: PaneId,
        event: &ScrollEvent,
        line_height: f32,
        cx: &mut Context<Self>,
    ) {
        if self.view_mode == ViewMode::Grid {
            return;
        }
        let delta = wheel_pixels(&event.delta, line_height);
        if delta.abs() < f32::EPSILON {
            return;
        }
        let now = Instant::now();
        // Touchpad deltas are already one frame of finger movement, so easing
        // them would only add lag; wheel notches get the glide.
        let precise = event.delta.precise();
        let reduced_motion = cx.reduce_motion();
        let max = self.list_scroll_max(pane);
        let smooth = self.pane_list_smooth(pane);
        smooth.set_max(max);
        if precise || reduced_motion {
            let next = smooth.offset() + delta;
            smooth.jump_to(next);
        } else {
            smooth.wheel(delta, now);
        }
        cx.notify();
    }

    /// Returns the offset to the top of the list without animation.
    pub fn reset_list_scroll(&mut self, pane: PaneId) {
        self.pane_list_smooth(pane).jump_to(0.0);
        self.pane_list_scroll(pane)
            .set_offset(point(px(0.0), px(0.0)));
    }

    /// Scrolls so the selected row stays inside the viewport.
    pub fn reveal_list_row(&mut self, pane: PaneId, index: usize, cx: &mut Context<Self>) {
        let max = self.list_scroll_max(pane);
        let viewport = match self.list_viewport_height(pane) {
            height if height > 0.0 => height,
            _ => return,
        };
        let offset = self.pane_list_smooth_ref(pane).offset();
        let target = keep_visible(offset, index, viewport, DETAILS_ROW_HEIGHT, max);
        let smooth = self.pane_list_smooth(pane);
        smooth.set_max(max);
        smooth.scroll_to(target, Instant::now());
        cx.notify();
    }

    /// Arms one frame callback that advances every pane's glide. Called from
    /// `render` while a glide runs, which keeps the easing on the display's frame
    /// clock: exactly one step per painted frame, no timer to beat against the
    /// vsync source and no wakeups once the glide has settled.
    fn arm_scroll_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.scroll_frame_armed {
            return;
        }
        self.scroll_frame_armed = true;
        let this = cx.entity();
        window.on_next_frame(move |window, cx| {
            this.update(cx, |explorer, cx| {
                explorer.scroll_frame_armed = false;
                if explorer.step_list_scroll(Instant::now(), cx) {
                    explorer.arm_scroll_frame(window, cx);
                } else {
                    // The settling step lands exactly on target without
                    // reporting progress, so repaint that last position.
                    cx.notify();
                }
            });
        });
    }

    /// Advances the scroll easing to `now`; returns `true` while it animates.
    fn step_list_scroll(&mut self, now: Instant, cx: &mut Context<Self>) -> bool {
        let mut running = false;
        for pane in [PaneId::Primary, PaneId::Secondary] {
            let max = self.list_scroll_max(pane);
            let smooth = self.pane_list_smooth(pane);
            smooth.set_max(max);
            running |= smooth.step(now);
            let offset = smooth.offset();
            self.pane_list_scroll(pane)
                .set_offset(point(px(0.0), px(-offset)));
        }
        if running {
            cx.notify();
        }
        running
    }

    #[must_use]
    pub fn active_pane_state(&self) -> PaneSnapshot<'_> {
        let pane = self.active_pane;
        PaneSnapshot {
            entries: self.pane_entries(pane),
            selected_index: self.pane_selected_index(pane),
        }
    }

    /// Directory driving the active pane; targets file operations and shell actions.
    #[must_use]
    pub(crate) fn active_directory(&self) -> PathBuf {
        self.pane_path(self.active_pane).to_path_buf()
    }

    pub fn set_active_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if self.active_pane == pane {
            return;
        }
        self.active_pane = pane;
        self.update_preview(cx);
        cx.notify();
    }

    pub fn toggle_dual_pane(&mut self, cx: &mut Context<Self>) {
        self.dual_pane = !self.dual_pane;
        self.layout_touched = true;
        if self.dual_pane {
            let path = self.secondary.path.clone();
            self.load_secondary_directory(path, true, cx);
        } else {
            self.active_pane = PaneId::Primary;
            self.secondary.cancel_token.cancel();
            self.secondary.folder_size_cancel.cancel();
            self.secondary.is_loading = false;
            self.secondary.active_folder_size_tasks = 0;
            self.update_preview(cx);
        }
        self.persist_session();
        cx.notify();
    }

    pub fn swap_panes(&mut self, cx: &mut Context<Self>) {
        if !self.dual_pane {
            return;
        }
        let primary = self.current_path().to_path_buf();
        let secondary = self.secondary.path.clone();
        if same_path(&primary, &secondary) {
            return;
        }
        self.load_directory(secondary, false, true, cx);
        self.load_secondary_directory(primary, true, cx);
        self.persist_session();
        cx.notify();
    }

    // ---- Splitter drag (Stage 2D) ----

    pub fn begin_splitter_drag(
        &mut self,
        kind: SplitterKind,
        x: f32,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let layout = splitter::compute_workspace_layout(
            self.viewport_size.0,
            self.sidebar_visible,
            self.sidebar_width,
            self.dual_pane,
            self.pane_split_ratio,
            self.preview_visible,
            self.preview_width,
        );
        self.splitter_drag = Some(ActiveSplitterDrag {
            kind,
            start_x: x,
            start_sidebar_w: self.sidebar_width,
            start_split_ratio: self.pane_split_ratio,
            start_preview_w: self.preview_width.unwrap_or(layout.preview_w),
            start_preview_pref: self.preview_width,
            files_area_w: layout.files_area_w,
        });
        cx.notify();
    }

    pub fn update_splitter_drag(&mut self, x: f32, cx: &mut Context<Self>) {
        let Some(drag) = self.splitter_drag else {
            return;
        };
        self.layout_touched = true;
        let delta = x - drag.start_x;
        match drag.kind {
            SplitterKind::Sidebar => {
                self.sidebar_width = splitter::apply_sidebar_drag(drag.start_sidebar_w, delta);
            }
            SplitterKind::DualPane => {
                self.pane_split_ratio = splitter::apply_dual_pane_drag(
                    drag.start_split_ratio,
                    drag.files_area_w,
                    delta,
                );
            }
            SplitterKind::Preview => {
                self.preview_width = Some(splitter::apply_preview_drag(
                    drag.start_preview_w,
                    delta,
                    false,
                ));
            }
        }
        cx.notify();
    }

    /// Ends a splitter drag and persists the resulting layout values.
    pub fn end_splitter_drag(&mut self, cx: &mut Context<Self>) {
        if self.splitter_drag.take().is_some() {
            self.persist_session();
            cx.notify();
        }
    }

    /// Aborts a splitter drag, restoring the values captured on mouse-down.
    fn cancel_splitter_drag(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.splitter_drag.take() else {
            return;
        };
        match drag.kind {
            SplitterKind::Sidebar => self.sidebar_width = drag.start_sidebar_w,
            SplitterKind::DualPane => self.pane_split_ratio = drag.start_split_ratio,
            SplitterKind::Preview => self.preview_width = drag.start_preview_pref,
        }
        cx.notify();
    }

    // ---- Bootstrap & background tasks ----

    fn spawn_bootstrap_sidebar_and_config(&mut self, cx: &mut Context<Self>) {
        let store = self.config_store.clone();
        let runtime = Arc::clone(&self.runtime);
        let (tx, rx) = tokio::sync::oneshot::channel();

        runtime.spawn_blocking(move || {
            let config = store.load_config();
            let db_path = store.dir().join("cache").join("folder_sizes.db");
            let size_store = FolderSizeStore::open(&db_path).ok().map(Arc::new);
            let known_folders = enumerate_known_folders();
            let drives = enumerate_drives();

            let mut icon_paths: Vec<(PathBuf, bool)> = Vec::new();
            for fav in &config.favorites {
                icon_paths.push((fav.path.clone(), true));
            }
            for folder in &known_folders {
                icon_paths.push((folder.path.clone(), true));
            }
            for drive in &drives {
                icon_paths.push((drive.root.clone(), true));
            }

            let mut sidebar_icons = Vec::with_capacity(icon_paths.len());
            for (path, is_dir) in icon_paths {
                let key = path_icon_key(&path);
                let icon = extract_path_icon_rgba(&path, is_dir);
                sidebar_icons.push((key, icon));
            }

            let session = store.load_session();
            let _ = tx.send((
                config,
                session,
                size_store,
                known_folders,
                drives,
                sidebar_icons,
            ));
        });

        cx.spawn(async move |this, cx| {
            let Ok((config, session, size_store, known_folders, drives, sidebar_icons)) = rx.await
            else {
                return;
            };
            let _ = this.update(cx, |explorer, cx| {
                explorer.favorites = config.favorites.clone();
                explorer.show_hidden = config.show_hidden;
                explorer.calculate_folder_sizes = config.calculate_folder_sizes;
                explorer.preview_visible = config.show_preview_pane;
                if matches!(config.default_view, ViewMode::Grid) {
                    explorer.view_mode = ViewMode::Grid;
                }
                if config.theme == ThemeMode::Light {
                    explorer.theme.dark_mode = false;
                }
                explorer.config = config;
                explorer.apply_session_layout(&session, cx);
                if explorer.config.indexed_search {
                    // The index lives on disk, so the previous session's entries
                    // answer queries right away and only the watcher is rebuilt.
                    explorer.set_indexed_search(true, cx);
                }
                explorer.folder_size_store = size_store;
                explorer.known_folders = known_folders;
                explorer.drives = drives;
                explorer.icons.insert_batch(sidebar_icons);

                // Ensure home path icon is extracted too if not in known_folders
                let home = explorer.home_path.clone();
                let home_key = path_icon_key(&home);
                if explorer.icons.needs_fetch(&home_key) {
                    explorer.icons.mark_pending(home_key.clone());
                    explorer.spawn_icon_batch(vec![(home_key, home, true)], cx);
                }

                // Trigger folder size pass & grid thumbnails if directory already loaded
                if !explorer.is_loading && !explorer.entries.is_empty() {
                    explorer.queue_folder_sizes(cx);
                    explorer.queue_thumbnails(cx);
                }
                explorer.update_preview(cx);
                cx.notify();
            });
        })
        .detach();
    }

    // ---- Tab operations ----

    pub fn new_tab(&mut self, cx: &mut Context<Self>) {
        let path = self.current_path().to_path_buf();
        self.tabs.new_tab(path.clone());
        self.sort_spec = self
            .tabs
            .active_tab()
            .map_or_else(SortSpec::default, |t| t.sort);
        self.load_directory(path, false, true, cx);
    }

    pub fn switch_tab(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.tabs().iter().position(|t| t.id == tab_id) else {
            return;
        };
        if index == self.tabs.active_index() {
            return;
        }
        if self.tabs.switch_tab(index)
            && let Some(tab) = self.tabs.active_tab().cloned()
        {
            self.sort_spec = tab.sort;
            self.address_editing = false;
            self.load_directory(tab.path, false, true, cx);
        }
    }

    pub fn close_tab(&mut self, tab_id: Option<u64>, cx: &mut Context<Self>) {
        let index = match tab_id {
            Some(id) => self.tabs.tabs().iter().position(|t| t.id == id),
            None => Some(self.tabs.active_index()),
        };
        let Some(index) = index else {
            return;
        };
        let prev_active = self.tabs.active_tab().map(|t| t.id);
        if !self.tabs.close_tab(index) {
            return;
        }
        if prev_active != self.tabs.active_tab().map(|t| t.id)
            && let Some(tab) = self.tabs.active_tab().cloned()
        {
            self.sort_spec = tab.sort;
            self.load_directory(tab.path, false, true, cx);
        } else {
            cx.notify();
        }
    }

    pub fn reorder_tab(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if self.tabs.reorder_tab(from, to) {
            cx.notify();
        }
    }

    // ---- Navigation & Directory Loading ----

    pub fn navigate_to(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if same_path(&path, self.current_path()) && !self.is_loading {
            return;
        }
        self.address_editing = false;
        self.address_suggestions.clear();
        self.load_directory(path, true, true, cx);
    }

    pub fn go_back(&mut self, cx: &mut Context<Self>) {
        let target = self
            .tabs
            .active_tab_mut()
            .and_then(|tab| tab.history.go_back().cloned());
        if let Some(target) = target {
            self.load_directory(target, false, true, cx);
        }
    }

    pub fn go_forward(&mut self, cx: &mut Context<Self>) {
        let target = self
            .tabs
            .active_tab_mut()
            .and_then(|tab| tab.history.go_forward().cloned());
        if let Some(target) = target {
            self.load_directory(target, false, true, cx);
        }
    }

    pub fn go_up(&mut self, cx: &mut Context<Self>) {
        let parent = self
            .pane_path(self.active_pane)
            .parent()
            .map(Path::to_path_buf);
        let Some(parent) = parent else {
            return;
        };
        if parent.as_os_str().is_empty() {
            return;
        }
        match self.active_pane {
            PaneId::Primary => self.load_directory(parent, true, true, cx),
            PaneId::Secondary => self.load_secondary_directory(parent, true, cx),
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        match self.active_pane {
            PaneId::Primary => {
                let path = self.current_path().to_path_buf();
                self.invalidate_path_caches(&path);
                self.load_directory(path, false, false, cx);
            }
            PaneId::Secondary => {
                let path = self.secondary.path.clone();
                self.invalidate_path_caches(&path);
                self.load_secondary_directory(path, false, cx);
            }
        }
    }

    fn invalidate_path_caches(&mut self, path: &Path) {
        self.directories.invalidate(path);
        self.folder_sizes.invalidate_prefix(path);
        self.folder_sizes.invalidate_ancestors(path);
        if let Some(store) = self.folder_size_store.clone() {
            let path_for_db = path.to_path_buf();
            self.runtime.spawn_blocking(move || {
                let _ = store.invalidate_with_ancestors(&path_for_db);
            });
        }
    }

    fn load_directory(
        &mut self,
        path: PathBuf,
        record_history: bool,
        use_cache: bool,
        cx: &mut Context<Self>,
    ) {
        if !same_path(&path, self.current_path()) {
            self.context_menu = None;
            self.reset_list_scroll(PaneId::Primary);
        }
        if self
            .watched_path
            .as_deref()
            .is_none_or(|watched| !same_path(watched, &path))
        {
            self.watched_path = Some(path.clone());
            let _ = self.watch_cmd_tx.send(path.clone());
        }

        self.cancel_token.cancel();
        self.folder_size_cancel.cancel();
        self.thumbnail_cancel.cancel();
        let cancel = CancellationToken::new();
        self.cancel_token = cancel.clone();
        self.folder_size_cancel = cancel.child_token();
        self.thumbnail_cancel = cancel.child_token();
        self.active_folder_size_tasks = 0;
        self.thumbnails.clear();

        self.generation = self.generation.next();
        let generation = self.generation;

        if let Some(tab) = self.tabs.active_tab_mut() {
            tab.navigate(path.clone(), record_history);
            self.sort_spec = tab.sort;
        }
        // Navigating away ends the search list. The query stays in the box and
        // keeps filtering the new folder, which is what the browse list does.
        if self.list_mode == ListMode::Search {
            self.cancel_list_search();
        }
        if self.indexed_search {
            self.index.watch(self.index_roots());
        }
        self.selected_index = None;
        self.error_message = None;
        self.is_loading = true;
        self.update_preview(cx);

        if use_cache && let Some(mut cached) = self.directories.get(&path) {
            sort_entries(&mut cached, self.sort_spec);
            self.all_entries = cached;
            self.apply_filter_and_sort();
            self.apply_pending_selection(cx);
            self.queue_entry_icons(cx);
            self.queue_thumbnails(cx);
        } else if use_cache {
            self.all_entries.clear();
            self.entries.clear();
        }

        self.ensure_led_animation(cx);
        cx.notify();

        let options = EnumerationOptions {
            show_hidden: self.show_hidden,
            show_system: self.config.show_system,
            sort: self.sort_spec,
            ..EnumerationOptions::default()
        };

        let runtime = Arc::clone(&self.runtime);
        let path_for_task = path.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        runtime.spawn(async move {
            let result = FsEngine::new()
                .enumerate_dir(path_for_task, generation, options, cancel)
                .await;
            let _ = tx.send(result);
        });

        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |explorer, cx| {
                if explorer.generation != generation {
                    return;
                }
                explorer.is_loading = false;
                match result {
                    Ok(mut entries) => {
                        sort_entries(&mut entries, explorer.sort_spec);
                        explorer.directories.put(path, entries.clone());
                        explorer.all_entries = entries;
                        explorer.error_message = None;
                        explorer.apply_filter_and_sort();
                        explorer.apply_pending_selection(cx);
                        explorer.queue_entry_icons(cx);
                        explorer.queue_folder_sizes(cx);
                        explorer.queue_thumbnails(cx);
                        explorer.update_preview(cx);
                    }
                    Err(err) => {
                        explorer.all_entries.clear();
                        explorer.entries.clear();
                        explorer.error_message = Some(err.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ---- Address Bar (%ENV% expansion + Autocomplete) ----

    pub fn begin_address_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.address_editing = true;
        self.address_suggestions.clear();
        self.suggestion_index = None;
        let display = normalize_display_path(self.current_path());
        self.address_input.update(cx, |input, cx| {
            input.set_value(display, window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    pub fn cancel_address_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.address_editing = false;
        self.address_suggestions.clear();
        self.suggestion_index = None;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn on_address_changed(&mut self, text: &str, cx: &mut Context<Self>) {
        if !self.address_editing {
            return;
        }
        self.address_generation = self.address_generation.wrapping_add(1);
        let addr_gen = self.address_generation;
        let expanded = expand_env_vars(text.trim());
        let Some((parent, prefix)) = split_autocomplete_prefix(&expanded) else {
            self.address_suggestions.clear();
            self.suggestion_index = None;
            cx.notify();
            return;
        };
        let parent = if parent.is_absolute() {
            parent
        } else {
            self.current_path().join(parent)
        };

        let runtime = Arc::clone(&self.runtime);
        let (tx, rx) = tokio::sync::oneshot::channel();
        runtime.spawn_blocking(move || {
            let suggestions = list_address_suggestions(&parent, &prefix);
            let _ = tx.send(suggestions);
        });

        cx.spawn(async move |this, cx| {
            let Ok(suggestions) = rx.await else {
                return;
            };
            let _ = this.update(cx, |explorer, cx| {
                if explorer.address_generation == addr_gen && explorer.address_editing {
                    explorer.address_suggestions = suggestions;
                    explorer.suggestion_index = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn handle_address_key_down(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = ev.keystroke.key.as_str();
        if key == "escape" {
            self.cancel_address_edit(window, cx);
            cx.stop_propagation();
            return;
        }
        if self.address_suggestions.is_empty() {
            return;
        }
        if key == "down" {
            let next = match self.suggestion_index {
                Some(idx) => (idx + 1).min(self.address_suggestions.len() - 1),
                None => 0,
            };
            self.suggestion_index = Some(next);
            cx.notify();
            cx.stop_propagation();
        } else if key == "up" {
            let prev = match self.suggestion_index {
                Some(idx) => idx.saturating_sub(1),
                None => 0,
            };
            self.suggestion_index = Some(prev);
            cx.notify();
            cx.stop_propagation();
        } else if key == "tab" {
            let idx = self.suggestion_index.unwrap_or(0);
            if let Some(chosen) = self.address_suggestions.get(idx).cloned() {
                self.accept_address_suggestion(&chosen, window, cx);
                cx.stop_propagation();
            }
        }
    }

    pub fn accept_address_suggestion(
        &mut self,
        suggestion: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let with_slash = if suggestion.ends_with('\\') {
            suggestion.to_string()
        } else {
            format!("{suggestion}\\")
        };
        self.address_input.update(cx, |input, cx| {
            input.set_value(with_slash.clone(), window, cx);
            input.focus(window, cx);
        });
        self.on_address_changed(&with_slash, cx);
    }

    pub fn submit_address(&mut self, raw: &str, window: &mut Window, cx: &mut Context<Self>) {
        let trimmed = raw.trim();
        let unquoted = trimmed
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(trimmed);
        self.address_editing = false;
        self.address_suggestions.clear();
        self.suggestion_index = None;
        window.focus(&self.focus_handle, cx);

        if unquoted.is_empty() {
            cx.notify();
            return;
        }

        let expanded = expand_env_vars(unquoted);
        let candidate = PathBuf::from(expanded.trim());
        let resolved = if candidate.is_absolute() {
            candidate
        } else {
            let joined = self.current_path().join(candidate);
            std::path::absolute(&joined).unwrap_or(joined)
        };
        self.load_directory(resolved, true, true, cx);
    }

    // ---- Search / Filter ----

    fn on_search_changed(&mut self, query: String, cx: &mut Context<Self>) {
        self.search_query = query;
        self.selected_index = None;
        self.secondary.selected_index = None;
        if self.indexed_search && !self.search_query.trim().is_empty() {
            self.start_list_search(true, cx);
            return;
        }
        self.cancel_list_search();
        self.apply_filter_and_sort();
        self.update_preview(cx);
        self.queue_thumbnails(cx);
        cx.notify();
    }

    pub fn clear_search_and_focus_main(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search_query.is_empty() {
            self.search_query.clear();
            self.search_input.update(cx, |input, cx| {
                input.set_value("", window, cx);
            });
            self.cancel_list_search();
            self.apply_filter_and_sort();
            self.update_preview(cx);
            self.queue_thumbnails(cx);
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Leaves the search list and restores the browse list of the open folders.
    fn cancel_list_search(&mut self) {
        self.search_run.cancel();
        self.search_hits.clear();
        self.list_mode = ListMode::Browse;
    }

    /// Starts one search run over the open folder tree and feeds the hits into
    /// the primary pane. The previous run is cancelled immediately, so a new
    /// keystroke aborts the old run and only the newest hits ever reach the list.
    fn start_list_search(&mut self, debounce: bool, cx: &mut Context<Self>) {
        let text = self.search_query.trim().to_string();
        if text.is_empty() {
            return;
        }
        let (generation, cancel) = self.search_run.restart(text.clone());
        self.list_mode = ListMode::Search;
        self.search_hits.clear();
        self.entries.clear();
        self.selected_index = None;
        self.error_message = None;

        let root = self.current_path().to_path_buf();
        let engine = self.search_engine.clone();
        let query = SearchQuery {
            text,
            include_hidden: self.show_hidden,
            use_index: self.search_index.is_some(),
            ..SearchQuery::default()
        };
        let (tx, rx) = oneshot::channel();
        self.runtime.spawn(async move {
            let result = search_index::run_query(engine, root, query, cancel, debounce).await;
            let _ = tx.send(result);
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |explorer, cx| {
                explorer.finish_list_search(generation, result, cx);
            });
        })
        .detach();

        self.ensure_led_animation(cx);
        cx.notify();
    }

    /// Applies the hits of `generation`; results of a superseded run are dropped.
    fn finish_list_search(
        &mut self,
        generation: GenerationId,
        result: OnyxResult<Vec<SearchResult>>,
        cx: &mut Context<Self>,
    ) {
        let Some(hits) = self.search_run.finish(generation, result) else {
            let status = self.search_run.status();
            if let SearchStatus::Failed { message } = status {
                self.error_message = Some(message.clone());
            }
            cx.notify();
            return;
        };
        // The index ignores the listing options of the browse view, so protect
        // the protected-system-file preference here, as the Slint app does.
        let show_system = self.config.show_system;
        self.search_hits = hits
            .into_iter()
            .filter(|entry| show_system || !entry.attributes.system)
            .collect();
        self.entries = self.search_hits.clone();
        self.selected_index = if self.entries.is_empty() {
            None
        } else {
            Some(0)
        };
        self.update_preview(cx);
        self.queue_entry_icons(cx);
        self.queue_thumbnails(cx);
        cx.notify();
    }

    pub fn toggle_indexed_search(&mut self, cx: &mut Context<Self>) {
        self.set_indexed_search(!self.indexed_search, cx);
    }

    /// Switches the persistent index on or off and persists the choice in
    /// `AppConfig`. Enabling opens `cache/search.sqlite` and indexes the open
    /// folder; disabling stops the watcher and falls back to live walks.
    pub fn set_indexed_search(&mut self, value: bool, cx: &mut Context<Self>) {
        if self.indexed_search == value {
            return;
        }
        self.indexed_search = value;
        self.config.indexed_search = value;
        self.persist_config();
        if value {
            self.index_status = IndexStatus::Opening;
            self.index
                .enable(self.index_roots(), self.config.excluded_index_dirs.clone());
            self.request_index_build(self.current_path().to_path_buf(), cx);
            self.status_message = Some("Indexed search on.".to_string());
        } else {
            self.index.disable();
            self.build_cancel.cancel();
            self.search_index = None;
            self.search_engine = UnifiedSearchEngine::live_only();
            self.index_status = IndexStatus::Disabled;
            self.rebuild_pending = None;
            self.cancel_list_search();
            self.apply_filter_and_sort();
            self.status_message = Some("Indexed search off.".to_string());
        }
        self.update_preview(cx);
        cx.notify();
    }

    /// Re-walks the open folder into the index on demand.
    pub fn rebuild_index(&mut self, cx: &mut Context<Self>) {
        if !self.indexed_search {
            self.set_indexed_search(true, cx);
            return;
        }
        let root = self.current_path().to_path_buf();
        self.status_message = Some(format!("Rebuilding index for {}…", root.display()));
        self.request_index_build(root, cx);
        cx.notify();
    }

    /// Roots the watcher keeps up to date: the home folder and the open one.
    fn index_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.home_path.clone()];
        let current = self.current_path().to_path_buf();
        if !roots.iter().any(|root| same_path(root, &current)) {
            roots.push(current);
        }
        roots
    }

    /// Short status text for the status bar: run state, matches, or nothing.
    #[must_use]
    pub fn search_status_label(&self) -> Option<String> {
        if self.search_query.trim().is_empty() {
            return None;
        }
        Some(match self.search_run.status() {
            SearchStatus::Running => "Searching…".to_string(),
            SearchStatus::Done { matches } => format!("{matches} matches"),
            SearchStatus::Failed { message } => format!("Search failed · {message}"),
            SearchStatus::Idle => format!("{} matches", self.entries.len()),
        })
    }

    // ---- Index lifecycle (worker events + blocking builds) ----

    /// Consumes the index worker's events on the GPUI thread.
    fn start_index_event_loop(
        &mut self,
        mut events: mpsc::UnboundedReceiver<IndexEvent>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                let alive = this
                    .update(cx, |explorer, cx| explorer.handle_index_event(event, cx))
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    fn handle_index_event(&mut self, event: IndexEvent, cx: &mut Context<Self>) {
        match event {
            IndexEvent::Ready { index } => {
                self.search_index = Some(Arc::clone(&index));
                self.search_engine = UnifiedSearchEngine::with_index(index);
                if !self.index_status.is_busy() {
                    self.index_status = IndexStatus::Watching { indexed: 0 };
                }
                if let Some(root) = self.rebuild_pending.take() {
                    self.start_index_build(root, cx);
                }
                // Queries that fell back to a live walk get a second chance now.
                self.restart_list_search_if_active(cx);
            }
            IndexEvent::Disabled => {
                self.search_index = None;
                self.search_engine = UnifiedSearchEngine::live_only();
                self.index_status = IndexStatus::Disabled;
            }
            IndexEvent::Failed { message } => {
                self.index_status = IndexStatus::Failed {
                    message: message.clone(),
                };
                self.status_message = Some(message);
            }
        }
        cx.notify();
    }

    fn restart_list_search_if_active(&mut self, cx: &mut Context<Self>) {
        if self.indexed_search && !self.search_query.trim().is_empty() {
            self.start_list_search(false, cx);
        }
    }

    /// Walks `root` into the index once it is open.
    fn request_index_build(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        if self.search_index.is_some() {
            self.start_index_build(root, cx);
        } else {
            // The worker still opens the database; build as soon as it is ready.
            self.rebuild_pending = Some(root);
            cx.notify();
        }
    }

    /// Runs `index_directory` plus a reconcile of `root` on the blocking pool,
    /// never on the UI thread.
    fn start_index_build(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let Some(shared) = self.search_index.clone() else {
            self.rebuild_pending = Some(root);
            return;
        };
        self.rebuild_pending = None;
        self.build_cancel.cancel();
        let cancel = CancellationToken::new();
        self.build_cancel = cancel.clone();
        self.build_generation = self.build_generation.next();
        let generation = self.build_generation;
        self.index_status = IndexStatus::Building { root: root.clone() };

        let build_root = root.clone();
        let (tx, rx) = oneshot::channel();
        self.runtime.spawn(async move {
            let indexed = tokio::task::spawn_blocking(move || -> OnyxResult<usize> {
                if cancel.is_cancelled() {
                    return Err(OnyxError::Cancelled);
                }
                let guard = shared.lock().map_err(|_| OnyxError::Search {
                    message: "search index mutex was poisoned".to_string(),
                })?;
                let count = guard.index_directory(&build_root, &cancel)?;
                if cancel.is_cancelled() {
                    return Err(OnyxError::Cancelled);
                }
                guard.reconcile_directory_with_cancel(&build_root, &cancel)?;
                Ok(count)
            })
            .await;
            let result = match indexed {
                Ok(inner) => inner,
                Err(error) => Err(OnyxError::Search {
                    message: format!("index worker failed: {error}"),
                }),
            };
            let _ = tx.send(result);
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |explorer, cx| {
                explorer.finish_index_build(generation, result, cx);
            });
        })
        .detach();

        self.ensure_led_animation(cx);
        cx.notify();
    }

    fn finish_index_build(
        &mut self,
        generation: GenerationId,
        result: OnyxResult<usize>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.build_generation {
            return;
        }
        match result {
            Ok(count) => {
                self.index_status = IndexStatus::Watching { indexed: count };
                self.status_message = Some(format!("Indexed {count} items."));
            }
            Err(OnyxError::Cancelled) => {
                self.index_status = IndexStatus::Watching { indexed: 0 };
            }
            Err(error) => {
                self.index_status = IndexStatus::Failed {
                    message: error.to_string(),
                };
                self.status_message = Some(format!("Indexing failed · {error}"));
            }
        }
        // The fresh entries can now answer the query without a live walk.
        self.restart_list_search_if_active(cx);
        cx.notify();
    }

    // ---- Sorting, View Mode, Favorites, Sidebar, Theme ----

    pub fn sort_by_in(&mut self, pane: PaneId, column: SortColumn, cx: &mut Context<Self>) {
        match pane {
            PaneId::Primary => {
                self.sort_spec.direction = if self.sort_spec.column == column {
                    self.sort_spec.direction.toggled()
                } else {
                    SortDirection::Ascending
                };
                self.sort_spec.column = column;
                if let Some(tab) = self.tabs.active_tab_mut() {
                    tab.sort = self.sort_spec;
                }
            }
            PaneId::Secondary => {
                self.secondary.sort_spec.direction = if self.secondary.sort_spec.column == column {
                    self.secondary.sort_spec.direction.toggled()
                } else {
                    SortDirection::Ascending
                };
                self.secondary.sort_spec.column = column;
            }
        }
        self.apply_filter_and_sort();
        cx.notify();
    }

    fn apply_filter_and_sort(&mut self) {
        // Search hits are ranked by the index and already filtered; the browse
        // lists must not overwrite them while a search is on screen.
        if self.list_mode == ListMode::Search {
            return;
        }
        let needle = self.search_query.trim().to_ascii_lowercase();
        self.entries = build_view_entries(
            &self.all_entries,
            self.sort_spec,
            &needle,
            &self.folder_sizes,
        );
        self.secondary.entries = build_view_entries(
            &self.secondary.all_entries,
            self.secondary.sort_spec,
            &needle,
            &self.folder_sizes,
        );
    }

    pub fn toggle_view_mode(&mut self, cx: &mut Context<Self>) {
        self.view_mode = match self.view_mode {
            ViewMode::Details | ViewMode::List => ViewMode::Grid,
            ViewMode::Grid => ViewMode::Details,
        };
        self.config.default_view = self.view_mode;
        self.persist_config();
        if self.view_mode == ViewMode::Grid {
            self.queue_thumbnails(cx);
        } else {
            self.thumbnail_cancel.cancel();
        }
        cx.notify();
    }

    pub fn toggle_hidden(&mut self, cx: &mut Context<Self>) {
        self.set_show_hidden(!self.show_hidden, cx);
    }

    pub fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_visible = !self.sidebar_visible;
        self.layout_touched = true;
        self.persist_session();
        cx.notify();
    }

    // ---- Preview Pane & Quick Look ----

    pub fn toggle_preview_pane(&mut self, cx: &mut Context<Self>) {
        self.preview_visible = !self.preview_visible;
        self.config.show_preview_pane = self.preview_visible;
        self.persist_config();
        self.update_preview(cx);
        cx.notify();
    }

    pub fn close_preview_pane(&mut self, cx: &mut Context<Self>) {
        if self.preview_visible {
            self.preview_visible = false;
            self.config.show_preview_pane = false;
            self.persist_config();
            self.update_preview(cx);
            cx.notify();
        }
    }

    pub fn toggle_quick_look(&mut self, cx: &mut Context<Self>) {
        if self.quick_look_open {
            self.close_quick_look(cx);
            return;
        }
        if self.overlay_open() {
            return;
        }
        let pane = self.active_pane;
        if self.pane_selected_index(pane).is_none() && !self.pane_entries(pane).is_empty() {
            match pane {
                PaneId::Primary => self.selected_index = Some(0),
                PaneId::Secondary => self.secondary.selected_index = Some(0),
            }
        }
        if self.pane_selected_index(pane).is_none() {
            return;
        }
        self.quick_look_open = true;
        self.update_preview(cx);
        cx.notify();
    }

    pub fn close_quick_look(&mut self, cx: &mut Context<Self>) {
        if self.quick_look_open {
            self.quick_look_open = false;
            self.update_preview(cx);
            cx.notify();
        }
    }

    pub fn open_selected_from_quick_look(&mut self, cx: &mut Context<Self>) {
        let pane = self.active_pane;
        let idx = self.pane_selected_index(pane);
        self.close_quick_look(cx);
        if let Some(idx) = idx {
            self.activate_entry_in(pane, idx, cx);
        }
    }

    fn update_preview(&mut self, cx: &mut Context<Self>) {
        let pane = self.active_pane;
        let entry = (self.preview_visible || self.quick_look_open)
            .then(|| {
                self.pane_selected_index(pane)
                    .and_then(|i| self.pane_entries(pane).get(i))
                    .cloned()
            })
            .flatten();

        let next_path = entry.as_ref().map(|e| &e.path);
        if next_path == self.preview.path.as_ref()
            && (next_path.is_none() || self.preview.kind != PreviewKind::None)
        {
            if next_path.is_none() && self.quick_look_open {
                self.quick_look_open = false;
            }
            return;
        }

        self.preview_cancel.cancel();
        let cancel = CancellationToken::new();
        self.preview_cancel = cancel.clone();
        self.preview_generation = self.preview_generation.next();
        let generation = self.preview_generation;

        if entry.is_none() && self.quick_look_open {
            self.quick_look_open = false;
        }
        self.preview.reset_for_entry(entry.as_ref());

        let Some(entry) = entry else {
            return;
        };

        let runtime = Arc::clone(&self.runtime);
        let path = entry.path;
        let (tx, rx) = oneshot::channel::<Result<DecodedPreview, OnyxError>>();
        runtime.spawn(async move {
            let result = PreviewEngine::new()
                .load_preview(path, cancel.clone())
                .await
                .map(DecodedPreview::from_content);
            let _ = tx.send(result);
        });

        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |explorer, cx| {
                if explorer.preview_generation != generation
                    || explorer.preview_cancel.is_cancelled()
                {
                    return;
                }
                match result {
                    Ok(decoded) => explorer.preview.apply_decoded(decoded),
                    Err(OnyxError::Cancelled) => return,
                    Err(err) => explorer.preview.apply_error(err.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn toggle_sidebar_group(&mut self, group_id: &str, cx: &mut Context<Self>) {
        if !self.collapsed_sidebar_groups.remove(group_id) {
            self.collapsed_sidebar_groups.insert(group_id.to_string());
        }
        cx.notify();
    }

    pub fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.theme.dark_mode = !self.theme.dark_mode;
        self.theme.sync_gpui_theme(Some(window), cx);
        self.config.theme = if self.theme.dark_mode {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        };
        self.persist_config();
        cx.notify();
    }

    pub fn toggle_favorite(&mut self, cx: &mut Context<Self>) {
        let current = self.current_path().to_path_buf();
        if self.config.remove_favorite(&current) {
            self.favorites = self.config.favorites.clone();
            self.persist_config();
            cx.notify();
            return;
        }

        let item = FavoriteItem {
            name: onyx_core::navigation::derive_tab_title(&current),
            path: current.clone(),
        };
        self.config.add_favorite(item);
        self.favorites = self.config.favorites.clone();
        self.persist_config();

        let key = path_icon_key(&current);
        if self.icons.needs_fetch(&key) {
            self.icons.mark_pending(key.clone());
            self.spawn_icon_batch(vec![(key, current, true)], cx);
        }
        cx.notify();
    }

    pub fn unpin_favorite(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.config.remove_favorite(path) {
            self.favorites = self.config.favorites.clone();
            self.persist_config();
            cx.notify();
        }
    }

    fn persist_config(&self) {
        let store = self.config_store.clone();
        let config = self.config.clone();
        self.runtime.spawn_blocking(move || {
            let _ = store.save_config(&config);
        });
    }

    /// Restores the persisted workspace layout (sidebar width, dual-pane state,
    /// preview width and split ratio) and reloads the secondary pane if needed.
    fn apply_session_layout(&mut self, session: &SessionState, cx: &mut Context<Self>) {
        // A layout the user already changed while the bootstrap was loading wins
        // over the persisted one.
        if !self.layout_touched {
            self.sidebar_visible = session.sidebar_visible;
            self.sidebar_width = session.sidebar_width;
            self.preview_width = session.preview_width;
            self.pane_split_ratio = session.pane_split_ratio;
            self.collapsed_sidebar_groups =
                session.collapsed_sidebar_groups.iter().cloned().collect();
            // The stored session wins; `dual_pane_default` only opens a second
            // pane when the session was saved without one.
            self.dual_pane = session.dual_pane || self.config.dual_pane_default;
        }
        if let Some(path) = session.secondary_path.clone() {
            self.secondary.path = path;
        }
        if self.dual_pane {
            let path = self.secondary.path.clone();
            self.load_secondary_directory(path, true, cx);
        }
        cx.notify();
    }

    /// Persists layout values asynchronously via `ConfigStore::save_session`.
    ///
    /// The stored session is re-read first so only the layout fields this view owns
    /// are replaced; tab state written by the other Onyx front ends survives.
    fn persist_session(&self) {
        let store = self.config_store.clone();
        let runtime = Arc::clone(&self.runtime);
        let dual_pane = self.dual_pane;
        let secondary_path = self.secondary.path.clone();
        let sidebar_visible = self.sidebar_visible;
        let sidebar_width = self.sidebar_width;
        let preview_width = self.preview_width;
        let pane_split_ratio = self.pane_split_ratio;
        let collapsed_groups: Vec<String> = self.collapsed_sidebar_groups.iter().cloned().collect();
        let (viewport_w, viewport_h) = self.viewport_size;
        let gate = Arc::clone(&self.session_write_gate);
        runtime.spawn_blocking(move || {
            // Serialize the read-modify-write so overlapping persists cannot
            // overwrite a newer layout snapshot with a stale one.
            let _guard = gate.lock();
            let mut session = store.load_session();
            session.dual_pane = dual_pane;
            session.secondary_path = Some(secondary_path);
            session.sidebar_visible = sidebar_visible;
            session.sidebar_width = sidebar_width;
            session.preview_width = preview_width;
            session.pane_split_ratio = pane_split_ratio;
            session.collapsed_sidebar_groups = collapsed_groups;
            if viewport_w > 1.0 && viewport_h > 1.0 {
                session.window_width = viewport_w.round() as u32;
                session.window_height = viewport_h.round() as u32;
            }
            session.sanitize();
            let _ = store.save_session(&session);
        });
    }

    // ---- Secondary pane loading (Stage 2D) ----

    fn load_secondary_directory(&mut self, path: PathBuf, use_cache: bool, cx: &mut Context<Self>) {
        self.reset_list_scroll(PaneId::Secondary);
        self.secondary.cancel_token.cancel();
        self.secondary.folder_size_cancel.cancel();
        let cancel = CancellationToken::new();
        self.secondary.cancel_token = cancel.clone();
        self.secondary.folder_size_cancel = cancel.child_token();
        self.secondary.active_folder_size_tasks = 0;
        self.secondary.generation = self.secondary.generation.next();
        let generation = self.secondary.generation;

        self.secondary.path = path.clone();
        self.secondary.selected_index = None;
        self.secondary.error_message = None;
        self.secondary.is_loading = true;
        self.sync_secondary_watcher();
        self.update_preview(cx);

        if use_cache && let Some(mut cached) = self.directories.get(&path) {
            sort_entries(&mut cached, self.secondary.sort_spec);
            self.secondary.all_entries = cached;
            self.apply_filter_and_sort();
            self.queue_entry_icons(cx);
            self.queue_thumbnails(cx);
        } else if use_cache {
            self.secondary.all_entries.clear();
            self.secondary.entries.clear();
        }

        cx.notify();

        let options = EnumerationOptions {
            show_hidden: self.show_hidden,
            show_system: self.config.show_system,
            sort: self.secondary.sort_spec,
            ..EnumerationOptions::default()
        };

        let runtime = Arc::clone(&self.runtime);
        let path_for_task = path.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        runtime.spawn(async move {
            let result = FsEngine::new()
                .enumerate_dir(path_for_task, generation, options, cancel)
                .await;
            let _ = tx.send(result);
        });

        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |explorer, cx| {
                if explorer.secondary.generation != generation {
                    return;
                }
                explorer.secondary.is_loading = false;
                match result {
                    Ok(mut entries) => {
                        sort_entries(&mut entries, explorer.secondary.sort_spec);
                        explorer.directories.put(path, entries.clone());
                        explorer.secondary.all_entries = entries;
                        explorer.secondary.error_message = None;
                        explorer.apply_filter_and_sort();
                        explorer.queue_entry_icons(cx);
                        explorer.queue_folder_sizes_for(PaneId::Secondary, cx);
                        explorer.queue_thumbnails(cx);
                        explorer.update_preview(cx);
                    }
                    Err(err) => {
                        explorer.secondary.all_entries.clear();
                        explorer.secondary.entries.clear();
                        explorer.secondary.error_message = Some(err.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn sync_secondary_watcher(&mut self) {
        if !self.dual_pane {
            return;
        }
        let path = self.secondary.path.clone();
        if self
            .secondary
            .watched_path
            .as_deref()
            .is_none_or(|watched| !same_path(watched, &path))
        {
            self.secondary.watched_path = Some(path.clone());
            let _ = self.watch_cmd_tx_secondary.send(path);
        }
    }

    // ---- Selection & Activation ----

    pub fn select_entry_in(&mut self, pane: PaneId, idx: usize, cx: &mut Context<Self>) {
        if idx >= self.pane_entries(pane).len() {
            return;
        }
        match pane {
            PaneId::Primary => self.selected_index = Some(idx),
            PaneId::Secondary => self.secondary.selected_index = Some(idx),
        }
        self.active_pane = pane;
        self.update_preview(cx);
        cx.notify();
    }

    pub fn activate_entry(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.activate_entry_in(self.active_pane, idx, cx);
    }

    pub fn activate_entry_in(&mut self, pane: PaneId, idx: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.pane_entries(pane).get(idx).cloned() else {
            return;
        };
        if entry.is_dir() {
            match pane {
                PaneId::Primary => self.load_directory(entry.path, true, true, cx),
                PaneId::Secondary => self.load_secondary_directory(entry.path, true, cx),
            }
        } else if entry_is_archive(&entry) {
            self.open_archive_browser(entry.path, cx);
        } else {
            let path = entry.path;
            self.runtime.spawn_blocking(move || {
                let _ = open_file(&path);
            });
        }
    }

    /// Opens a search hit: folders are entered, files reveal their parent
    /// folder with the hit selected.
    fn reveal_entry(&mut self, entry: FileEntry, window: &mut Window, cx: &mut Context<Self>) {
        if entry.is_dir() {
            self.navigate_to(entry.path, cx);
        } else {
            let parent = entry
                .path
                .parent()
                .map_or_else(|| entry.path.clone(), Path::to_path_buf);
            self.pending_selection = Some(entry.path.clone());
            self.navigate_to(parent, cx);
        }
        window.focus(&self.focus_handle, cx);
    }

    /// Selects and reveals the hit the palette asked for once `entries` holds it.
    fn apply_pending_selection(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.pending_selection.clone() else {
            return;
        };
        let Some(index) = self
            .entries
            .iter()
            .position(|entry| same_path(&entry.path, &path))
        else {
            return;
        };
        self.pending_selection = None;
        self.selected_index = Some(index);
        if self.view_mode != ViewMode::Grid {
            self.reveal_list_row(PaneId::Primary, index, cx);
        }
        self.update_preview(cx);
    }

    fn move_selection(&mut self, offset: isize, cx: &mut Context<Self>) {
        let pane = self.active_pane;
        let count = self.pane_entries(pane).len();
        if count == 0 {
            return;
        }
        let next = match self.pane_selected_index(pane) {
            Some(idx) => idx.saturating_add_signed(offset).min(count - 1),
            None => {
                if offset < 0 {
                    count - 1
                } else {
                    0
                }
            }
        };
        match pane {
            PaneId::Primary => self.selected_index = Some(next),
            PaneId::Secondary => self.secondary.selected_index = Some(next),
        }
        if self.view_mode != ViewMode::Grid {
            self.reveal_list_row(pane, next, cx);
        }
        self.update_preview(cx);
        cx.notify();
    }

    // ---- Async Grid Thumbnails (ThumbnailEngine, max 4 workers) ----

    fn queue_thumbnails(&mut self, cx: &mut Context<Self>) {
        if self.view_mode != ViewMode::Grid {
            self.thumbnail_cancel.cancel();
            return;
        }

        self.thumbnail_cancel.cancel();
        let cancel = self.cancel_token.child_token();
        self.thumbnail_cancel = cancel.clone();
        self.thumbnail_generation = self.thumbnail_generation.next();
        let thumb_gen = self.thumbnail_generation;

        let mut panes = vec![PaneId::Primary];
        if self.dual_pane {
            panes.push(PaneId::Secondary);
        }

        let mut pending = VecDeque::new();
        for pane in panes {
            for entry in self.pane_entries(pane) {
                if entry.is_dir()
                    || !is_thumbnail_supported(&entry.path)
                    || self.thumbnails.contains_key(&entry.path)
                {
                    continue;
                }
                pending.push_back(entry.path.clone());
            }
        }

        if pending.is_empty() {
            return;
        }

        let worker_count = pending.len().min(MAX_THUMBNAIL_WORKERS);
        let shared_queue = Arc::new(Mutex::new(pending));
        let (tx, mut rx) = mpsc::unbounded_channel::<(PathBuf, Arc<RenderImage>)>();

        for _ in 0..worker_count {
            let queue = Arc::clone(&shared_queue);
            let engine = self.thumbnail_engine.clone();
            let cancel_tok = cancel.clone();
            let tx_worker = tx.clone();
            self.runtime.spawn(async move {
                loop {
                    if cancel_tok.is_cancelled() {
                        break;
                    }
                    let next_path = {
                        let Ok(mut guard) = queue.lock() else {
                            break;
                        };
                        guard.pop_front()
                    };
                    let Some(path) = next_path else {
                        break;
                    };
                    match engine
                        .load_thumbnail(path.clone(), THUMBNAIL_TARGET_PX, cancel_tok.clone())
                        .await
                    {
                        Ok(thumb) => {
                            if cancel_tok.is_cancelled() {
                                break;
                            }
                            if let Some(render_img) =
                                rgba_pixels_to_render_image(thumb.width, thumb.height, &thumb.rgba)
                                && tx_worker.send((path, render_img)).is_err()
                            {
                                break;
                            }
                        }
                        Err(OnyxError::Cancelled) => break,
                        Err(_) => {}
                    }
                }
            });
        }
        drop(tx);

        cx.spawn(async move |this, cx| {
            while let Some((path, render_img)) = rx.recv().await {
                let keep_going = this
                    .update(cx, |explorer, cx| {
                        if explorer.thumbnail_generation != thumb_gen
                            || explorer.view_mode != ViewMode::Grid
                            || explorer.thumbnail_cancel.is_cancelled()
                        {
                            return false;
                        }
                        explorer.thumbnails.insert(path, render_img);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    // ---- Async Windows Shell Icons ----

    fn queue_entry_icons(&mut self, cx: &mut Context<Self>) {
        let mut candidates: HashMap<String, (PathBuf, bool)> = HashMap::new();
        let mut panes = vec![PaneId::Primary];
        if self.dual_pane {
            panes.push(PaneId::Secondary);
        }
        for pane in panes {
            for entry in self.pane_entries(pane) {
                let key = entry_icon_key(entry);
                if self.icons.needs_fetch(&key) {
                    candidates
                        .entry(key)
                        .or_insert_with(|| (entry.path.clone(), entry.is_dir()));
                }
            }
        }
        if candidates.is_empty() {
            return;
        }
        let work: Vec<(String, PathBuf, bool)> = candidates
            .into_iter()
            .map(|(k, (p, d))| {
                self.icons.mark_pending(k.clone());
                (k, p, d)
            })
            .collect();
        self.spawn_icon_batch(work, cx);
    }

    fn spawn_icon_batch(&self, work: Vec<(String, PathBuf, bool)>, cx: &mut Context<Self>) {
        let runtime = Arc::clone(&self.runtime);
        let (tx, mut rx) =
            tokio::sync::mpsc::unbounded_channel::<Vec<(String, Option<RgbaIcon>)>>();
        runtime.spawn_blocking(move || {
            let mut batch = Vec::with_capacity(24);
            for (key, path, is_dir) in work {
                let icon = if key.starts_with("path:") {
                    extract_path_icon_rgba(&path, is_dir)
                } else {
                    extract_file_icon_rgba(&path, is_dir)
                };
                batch.push((key, icon));
                if batch.len() == 24 {
                    let filled = std::mem::replace(&mut batch, Vec::with_capacity(24));
                    if tx.send(filled).is_err() {
                        return;
                    }
                }
            }
            if !batch.is_empty() {
                let _ = tx.send(batch);
            }
        });

        cx.spawn(async move |this, cx| {
            while let Some(batch) = rx.recv().await {
                let updated = this
                    .update(cx, |explorer, cx| {
                        explorer.icons.insert_batch(batch);
                        cx.notify();
                    })
                    .is_ok();
                if !updated {
                    break;
                }
            }
        })
        .detach();
    }

    // ---- Async Folder Size Calculation (onyx-cache + onyx-fs) ----

    fn queue_folder_sizes(&mut self, cx: &mut Context<Self>) {
        self.queue_folder_sizes_for(PaneId::Primary, cx);
        if self.dual_pane {
            self.queue_folder_sizes_for(PaneId::Secondary, cx);
        }
    }

    fn queue_folder_sizes_for(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if !self.calculate_folder_sizes {
            return;
        }

        let cancel = match pane {
            PaneId::Primary => {
                self.folder_size_cancel.cancel();
                self.folder_size_cancel = self.cancel_token.child_token();
                self.folder_size_cancel.clone()
            }
            PaneId::Secondary => {
                self.secondary.folder_size_cancel.cancel();
                self.secondary.folder_size_cancel = self.secondary.cancel_token.child_token();
                self.secondary.folder_size_cancel.clone()
            }
        };
        let generation = match pane {
            PaneId::Primary => self.generation,
            PaneId::Secondary => self.secondary.generation,
        };

        let mut pending: Vec<(PathBuf, Option<i64>)> = Vec::new();
        let mut any_cached = false;

        match pane {
            PaneId::Primary => {
                let cached_sizes = &self.folder_sizes;
                for entry in &mut self.all_entries {
                    if !entry.is_dir() || entry.is_symlink || entry.attributes.reparse_point {
                        continue;
                    }
                    if let Some(cached) = cached_sizes.get(&entry.path) {
                        if entry.size != cached.total_bytes {
                            entry.size = cached.total_bytes;
                            any_cached = true;
                        }
                    } else {
                        pending.push((entry.path.clone(), entry.modified));
                    }
                }
            }
            PaneId::Secondary => {
                let cached_sizes = &self.folder_sizes;
                for entry in &mut self.secondary.all_entries {
                    if !entry.is_dir() || entry.is_symlink || entry.attributes.reparse_point {
                        continue;
                    }
                    if let Some(cached) = cached_sizes.get(&entry.path) {
                        if entry.size != cached.total_bytes {
                            entry.size = cached.total_bytes;
                            any_cached = true;
                        }
                    } else {
                        pending.push((entry.path.clone(), entry.modified));
                    }
                }
            }
        }

        if any_cached {
            self.apply_filter_and_sort();
            cx.notify();
        }

        if pending.is_empty() {
            return;
        }

        match pane {
            PaneId::Primary => {
                self.active_folder_size_tasks =
                    self.active_folder_size_tasks.saturating_add(pending.len());
            }
            PaneId::Secondary => {
                self.secondary.active_folder_size_tasks = self
                    .secondary
                    .active_folder_size_tasks
                    .saturating_add(pending.len());
            }
        }
        self.ensure_led_animation(cx);

        let semaphore = Arc::new(Semaphore::new(MAX_FOLDER_SIZE_CONCURRENCY));
        let store = self.folder_size_store.clone();
        let runtime = Arc::clone(&self.runtime);

        for (dir_path, entry_modified) in pending {
            let sem = Arc::clone(&semaphore);
            let cancel_tok = cancel.clone();
            let store_opt = store.clone();
            let (tx, rx) = tokio::sync::oneshot::channel::<Option<(PathBuf, FolderSizeResult)>>();

            runtime.spawn(async move {
                if cancel_tok.is_cancelled() {
                    let _ = tx.send(None);
                    return;
                }

                let mtime_secs = if let Some(store_ref) = &store_opt {
                    let path_db = dir_path.clone();
                    let store_db = Arc::clone(store_ref);
                    let lookup = tokio::task::spawn_blocking(move || {
                        let mtime = entry_modified.or_else(|| dir_mtime_secs(&path_db));
                        let cached =
                            mtime.and_then(|ts| store_db.get_if_fresh(&path_db, ts).ok().flatten());
                        (mtime, cached)
                    })
                    .await;

                    match lookup {
                        Ok((_, Some(cached))) => {
                            let res = FolderSizeResult {
                                total_bytes: cached.total_bytes,
                                file_count: cached.file_count,
                                truncated: cached.truncated,
                            };
                            let _ = tx.send(Some((dir_path, res)));
                            return;
                        }
                        Ok((mtime, None)) => mtime,
                        Err(_) => entry_modified,
                    }
                } else {
                    entry_modified
                };

                let Ok(permit) = sem.acquire_owned().await else {
                    let _ = tx.send(None);
                    return;
                };
                if cancel_tok.is_cancelled() {
                    drop(permit);
                    let _ = tx.send(None);
                    return;
                }

                let result = compute_folder_size(
                    dir_path.clone(),
                    DEFAULT_MAX_FOLDER_SIZE_ENTRIES,
                    cancel_tok.clone(),
                )
                .await;
                drop(permit);

                match result {
                    Ok(size) => {
                        if let Some(store_ref) = store_opt
                            && let Some(mtime) = mtime_secs
                        {
                            let path_db = dir_path.clone();
                            let cached = CachedFolderSize::new(
                                size.total_bytes,
                                size.file_count,
                                size.truncated,
                            );
                            let _ = tokio::task::spawn_blocking(move || {
                                let _ = store_ref.upsert(&path_db, cached, mtime);
                            })
                            .await;
                        }
                        let _ = tx.send(Some((dir_path, size)));
                    }
                    Err(_) => {
                        let _ = tx.send(None);
                    }
                }
            });

            cx.spawn(async move |this, cx| {
                let Ok(outcome) = rx.await else {
                    return;
                };
                let _ = this.update(cx, |explorer, cx| {
                    let pane_generation = match pane {
                        PaneId::Primary => explorer.generation,
                        PaneId::Secondary => explorer.secondary.generation,
                    };
                    if pane_generation != generation {
                        return;
                    }
                    match pane {
                        PaneId::Primary => {
                            explorer.active_folder_size_tasks =
                                explorer.active_folder_size_tasks.saturating_sub(1);
                        }
                        PaneId::Secondary => {
                            explorer.secondary.active_folder_size_tasks = explorer
                                .secondary
                                .active_folder_size_tasks
                                .saturating_sub(1);
                        }
                    }
                    if let Some((path, size)) = outcome {
                        let cached = CachedFolderSize::new(
                            size.total_bytes,
                            size.file_count,
                            size.truncated,
                        );
                        explorer.folder_sizes.put(&path, cached);
                        let all_entries = match pane {
                            PaneId::Primary => &mut explorer.all_entries,
                            PaneId::Secondary => &mut explorer.secondary.all_entries,
                        };
                        for entry in all_entries {
                            if same_path(&entry.path, &path) {
                                entry.size = size.total_bytes;
                            }
                        }
                        explorer.apply_filter_and_sort();
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }

    // ---- LED Dot-Matrix Animation (zero idle timer wakeups) ----

    fn ensure_led_animation(&mut self, cx: &mut Context<Self>) {
        if self.anim_running || !self.is_activity_active() {
            return;
        }
        self.anim_running = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(65))
                    .await;
                let keep_going = this
                    .update(cx, |explorer, cx| {
                        if !explorer.is_activity_active() {
                            explorer.anim_running = false;
                            cx.notify();
                            return false;
                        }
                        explorer.anim_frame = explorer.anim_frame.wrapping_add(1);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    // ---- Clipboard Monitor & Async File Operations ----

    fn start_clipboard_monitor(
        &self,
        mut commands: mpsc::UnboundedReceiver<ClipboardCommand>,
        cx: &mut Context<Self>,
    ) {
        let runtime = Arc::clone(&self.runtime);
        let handle = runtime.handle().clone();
        let shutdown = self.shutdown_token.clone();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel::<ClipboardUpdate>();

        runtime.spawn_blocking(move || {
            let mut clipboard = ClipboardState::default();
            let mut last_has_files = false;
            let mut timer =
                handle.block_on(async { tokio::time::interval(Duration::from_millis(800)) });
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                let command = handle.block_on(async {
                    tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => None,
                        command = commands.recv() => command.map(Some),
                        _ = timer.tick() => Some(None),
                    }
                });
                match command {
                    None => break,
                    Some(Some(ClipboardCommand::Write(files))) => {
                        let title = if files.1 { "Cut" } else { "Copied" };
                        let message = clipboard
                            .write(files)
                            .unwrap_or_else(|| format!("{title} to clipboard."));
                        let _ = update_tx.send(ClipboardUpdate::Status(message));
                    }
                    Some(Some(ClipboardCommand::Read(reply))) => {
                        let _ = reply.send(clipboard.read());
                    }
                    Some(Some(ClipboardCommand::Moved { original, moved })) => {
                        if clipboard
                            .read()
                            .is_some_and(|(paths, cut)| cut && paths == original)
                        {
                            clipboard.consumed.extend(moved);
                        }
                    }
                    Some(None) => {}
                }
                let has_files = clipboard.read().is_some();
                if has_files != last_has_files {
                    last_has_files = has_files;
                    let _ = update_tx.send(ClipboardUpdate::CanPaste(has_files));
                }
            }
        });

        cx.spawn(async move |this, cx| {
            while let Some(update) = update_rx.recv().await {
                let ok = this
                    .update(cx, |explorer, cx| {
                        match update {
                            ClipboardUpdate::CanPaste(can_paste) => {
                                explorer.can_paste = can_paste;
                                if let Some(menu) = &mut explorer.context_menu {
                                    menu.can_paste = can_paste;
                                }
                            }
                            ClipboardUpdate::Status(msg) => {
                                explorer.status_message = Some(msg);
                            }
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !ok {
                    break;
                }
            }
        })
        .detach();
    }

    pub fn prompt_new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let parent = self.active_directory();
        self.open_prompt(
            PromptKind::NewFolder(parent),
            "New folder",
            "Enter a name for the folder.",
            "",
            "Create",
            window,
            cx,
        );
    }

    pub fn prompt_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        self.open_prompt(
            PromptKind::Rename(entry.path),
            "Rename",
            "Enter a new name.",
            &entry.name,
            "Rename",
            window,
            cx,
        );
    }

    pub fn prompt_permanent_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        let message = format!(
            "Permanently delete \"{}\"? This cannot be undone.",
            entry.name
        );
        self.open_prompt(
            PromptKind::PermanentDelete(entry.path),
            "Permanently delete?",
            &message,
            "",
            "Delete permanently",
            window,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn open_prompt(
        &mut self,
        kind: PromptKind,
        title: &str,
        message: &str,
        initial_input: &str,
        confirm_label: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_operation.is_some() {
            self.status_message = Some("A file operation is already in progress.".to_string());
            cx.notify();
            return;
        }
        let is_danger = matches!(kind, PromptKind::PermanentDelete(_));
        let show_input = !is_danger;
        self.prompt_state = Some(PromptState {
            kind,
            title: title.to_string(),
            message: message.to_string(),
            confirm_label: confirm_label.to_string(),
            is_danger,
            show_input,
        });
        if show_input {
            let value = initial_input.to_string();
            self.prompt_input.update(cx, |input, cx| {
                input.set_value(value, window, cx);
                input.focus(window, cx);
            });
        } else {
            window.focus(&self.prompt_focus_handle, cx);
        }
        cx.notify();
    }

    pub fn cancel_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_state = None;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    pub fn confirm_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = self.prompt_state.take() else {
            return;
        };
        let input_text = self.prompt_input.read(cx).value().to_string();
        window.focus(&self.focus_handle, cx);

        let operation = match prompt.kind {
            PromptKind::NewFolder(parent) => FileOperation::Create {
                parent,
                name: input_text,
            },
            PromptKind::Rename(source) => FileOperation::Rename {
                source,
                name: input_text,
            },
            PromptKind::PermanentDelete(path) => {
                FileOperation::PermanentDelete(PermanentDeleteRequest::confirmed(vec![path]))
            }
        };
        self.start_operation(operation, cx);
    }

    pub fn cut_selection(&mut self, cx: &mut Context<Self>) {
        self.write_selection_to_clipboard(true, cx);
    }

    pub fn copy_selection(&mut self, cx: &mut Context<Self>) {
        self.write_selection_to_clipboard(false, cx);
    }

    fn write_selection_to_clipboard(&mut self, cut: bool, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        self.can_paste = true;
        let _ = self
            .clipboard_tx
            .send(ClipboardCommand::Write((vec![entry.path], cut)));
        cx.notify();
    }

    pub fn paste(&mut self, cx: &mut Context<Self>) {
        let destination = self.active_directory();
        self.start_operation(FileOperation::Paste(destination), cx);
    }

    pub fn duplicate_selection(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        self.start_operation(FileOperation::Duplicate(entry.path), cx);
    }

    /// Moves the selected item to the Windows Recycle Bin (default safe delete).
    pub fn recycle_selection(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        self.start_operation(FileOperation::Recycle(entry.path), cx);
    }

    pub fn undo(&mut self, cx: &mut Context<Self>) {
        self.start_operation(FileOperation::Undo, cx);
    }

    pub fn cancel_operation(&mut self, cx: &mut Context<Self>) {
        if let Some(op) = &self.active_operation {
            op.cancel.cancel();
            self.status_message = Some("Cancelling file operation…".to_string());
            cx.notify();
        }
    }

    fn start_operation(&mut self, operation: FileOperation, cx: &mut Context<Self>) {
        if self.active_operation.is_some() {
            self.status_message = Some("A file operation is already in progress.".to_string());
            cx.notify();
            return;
        }
        let title = operation.title();
        let is_undo = matches!(operation, FileOperation::Undo);
        let cancel = CancellationToken::new();
        self.status_message = None;
        self.active_operation = Some(ActiveOperation {
            title,
            current_item: String::new(),
            progress: 0.0,
            detail: String::new(),
            cancel: cancel.clone(),
        });
        self.ensure_led_animation(cx);
        cx.notify();

        let engine = Arc::clone(&self.operations);
        let clipboard = self.clipboard_tx.clone();
        let runtime = Arc::clone(&self.runtime);
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<OperationEvent>();

        runtime.spawn(async move {
            let (progress_tx, mut progress_rx) = mpsc::channel(16);
            let drain_tx = event_tx.clone();
            let drain = async move {
                while let Some(progress) = progress_rx.recv().await {
                    let _ = drain_tx.send(OperationEvent::Progress(progress));
                }
            };
            let mut affected = Vec::new();
            let (result, ()) = tokio::join!(
                operation.run(
                    Arc::clone(&engine),
                    clipboard,
                    cancel,
                    progress_tx,
                    &mut affected,
                ),
                drain,
            );
            let can_undo = engine.can_undo().await;
            let _ = event_tx.send(OperationEvent::Finished {
                result,
                can_undo,
                affected,
                is_undo,
            });
        });

        cx.spawn(async move |this, cx| {
            while let Some(event) = event_rx.recv().await {
                let ok = this
                    .update(cx, |explorer, cx| match event {
                        OperationEvent::Progress(progress) => {
                            if let Some(active) = &mut explorer.active_operation {
                                active.progress = progress_fraction(&progress);
                                active.current_item = progress.current_item;
                                active.detail = progress.error.unwrap_or_else(|| {
                                    format!(
                                        "{} of {} items",
                                        progress.completed_items, progress.total_items
                                    )
                                });
                            }
                            cx.notify();
                        }
                        OperationEvent::Finished {
                            result,
                            can_undo,
                            affected,
                            is_undo,
                        } => {
                            explorer.active_operation = None;
                            explorer.can_undo = can_undo;
                            explorer.status_message = Some(operation_message(title, result));
                            if is_undo {
                                explorer.directories.clear();
                                explorer.refresh(cx);
                            } else {
                                explorer.refresh_affected_paths(&affected, cx);
                            }
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !ok {
                    break;
                }
            }
        })
        .detach();
    }

    fn refresh_affected_paths(&mut self, affected: &[PathBuf], cx: &mut Context<Self>) {
        let mut reload_current = false;
        let current = self.current_path().to_path_buf();

        for path in affected {
            self.directories.invalidate(path);
            self.folder_sizes.invalidate_prefix(path);
            self.folder_sizes.invalidate_ancestors(path);
            if let Some(store) = self.folder_size_store.clone() {
                let path_for_db = path.clone();
                self.runtime.spawn_blocking(move || {
                    let _ = store.invalidate_with_ancestors(&path_for_db);
                });
            }
            if same_path(path, &current) {
                reload_current = true;
            }
        }

        if reload_current {
            self.load_directory(current, false, false, cx);
        }
        if self.dual_pane {
            let secondary = self.secondary.path.clone();
            if affected.iter().any(|path| same_path(path, &secondary)) {
                self.load_secondary_directory(secondary, false, cx);
            }
        }
    }

    pub fn open_terminal(&mut self, cx: &mut Context<Self>) {
        let path = self.active_directory();
        let profile = terminal_profile_from_config(self.config.normalized_terminal());
        self.spawn_shell_task(
            path,
            move |dir| launch_terminal_with_profile(dir, profile),
            cx,
        );
    }

    pub fn show_properties(&mut self, cx: &mut Context<Self>) {
        let path = self
            .selected_entry()
            .map_or_else(|| self.active_directory(), |e| e.path.clone());
        self.spawn_shell_task(path, show_properties_dialog, cx);
    }

    fn spawn_shell_task(
        &self,
        path: PathBuf,
        action: impl FnOnce(&Path) -> OnyxResult<()> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let runtime = Arc::clone(&self.runtime);
        let (tx, rx) = oneshot::channel::<String>();
        runtime.spawn_blocking(move || {
            if let Err(error) = action(&path) {
                let _ = tx.send(error.to_string());
            }
        });
        cx.spawn(async move |this, cx| {
            let Ok(message) = rx.await else {
                return;
            };
            let _ = this.update(cx, |explorer, cx| {
                explorer.status_message = Some(message);
                cx.notify();
            });
        })
        .detach();
    }

    // ---- Context Menu & Live Filesystem Watcher (Stage 2C) ----

    pub fn open_in_new_tab(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.tabs.new_tab(path.clone());
        self.sort_spec = self
            .tabs
            .active_tab()
            .map_or_else(SortSpec::default, |t| t.sort);
        self.load_directory(path, false, true, cx);
    }

    pub fn open_item_context_menu(
        &mut self,
        idx: usize,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let pane = self.active_pane;
        let Some(entry) = self.pane_entries(pane).get(idx).cloned() else {
            return;
        };
        let is_dir = entry.is_dir();
        self.select_entry_in(pane, idx, cx);
        self.context_menu = Some(ContextMenuState::new(
            ContextMenuTarget::Item {
                index: idx,
                path: entry.path,
                is_dir,
            },
            f32::from(position.x),
            f32::from(position.y),
            self.can_paste,
        ));
        cx.notify();
    }

    pub fn open_background_context_menu(
        &mut self,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.context_menu = Some(ContextMenuState::new(
            ContextMenuTarget::Background {
                dir_path: self.active_directory(),
            },
            f32::from(position.x),
            f32::from(position.y),
            self.can_paste,
        ));
        cx.notify();
    }

    pub fn close_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.take().is_some() {
            cx.notify();
        }
    }

    pub fn execute_context_menu_action(
        &mut self,
        action: ContextMenuAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.context_menu.take() else {
            return;
        };
        cx.notify();

        match (action, state.target) {
            (ContextMenuAction::Open, ContextMenuTarget::Item { index, .. }) => {
                self.activate_entry(index, cx);
            }
            (
                ContextMenuAction::OpenInNewTab,
                ContextMenuTarget::Item {
                    path, is_dir: true, ..
                },
            ) => {
                self.open_in_new_tab(path, cx);
            }
            (ContextMenuAction::OpenWith, ContextMenuTarget::Item { path, .. }) => {
                self.spawn_shell_task(path, open_with_dialog, cx);
            }
            (ContextMenuAction::Cut, ContextMenuTarget::Item { path, .. }) => {
                self.can_paste = true;
                let _ = self
                    .clipboard_tx
                    .send(ClipboardCommand::Write((vec![path], true)));
                cx.notify();
            }
            (ContextMenuAction::Copy, ContextMenuTarget::Item { path, .. }) => {
                self.can_paste = true;
                let _ = self
                    .clipboard_tx
                    .send(ClipboardCommand::Write((vec![path], false)));
                cx.notify();
            }
            (ContextMenuAction::Paste, ContextMenuTarget::Item { path, is_dir, .. }) => {
                let destination = if is_dir {
                    path
                } else {
                    self.active_directory()
                };
                self.start_operation(FileOperation::Paste(destination), cx);
            }
            (ContextMenuAction::Paste, ContextMenuTarget::Background { dir_path }) => {
                self.start_operation(FileOperation::Paste(dir_path), cx);
            }
            (ContextMenuAction::CopyPath, ContextMenuTarget::Item { path, .. }) => {
                let display = normalize_display_path(&path);
                cx.write_to_clipboard(ClipboardItem::new_string(display));
                self.status_message = Some("Copied path to clipboard.".to_string());
                cx.notify();
            }
            (ContextMenuAction::Rename, ContextMenuTarget::Item { path, .. }) => {
                let initial = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.open_prompt(
                    PromptKind::Rename(path),
                    "Rename",
                    "Enter a new name.",
                    &initial,
                    "Rename",
                    window,
                    cx,
                );
            }
            (ContextMenuAction::Duplicate, ContextMenuTarget::Item { path, .. }) => {
                self.start_operation(FileOperation::Duplicate(path), cx);
            }
            (ContextMenuAction::OpenInTerminal, ContextMenuTarget::Item { path, is_dir, .. }) => {
                let dir = if is_dir {
                    path
                } else {
                    self.active_directory()
                };
                let profile = terminal_profile_from_config(self.config.normalized_terminal());
                self.spawn_shell_task(
                    dir,
                    move |target| launch_terminal_with_profile(target, profile),
                    cx,
                );
            }
            (ContextMenuAction::OpenInTerminal, ContextMenuTarget::Background { dir_path }) => {
                let profile = terminal_profile_from_config(self.config.normalized_terminal());
                self.spawn_shell_task(
                    dir_path,
                    move |target| launch_terminal_with_profile(target, profile),
                    cx,
                );
            }
            (ContextMenuAction::Properties, ContextMenuTarget::Item { path, .. }) => {
                self.spawn_shell_task(path, show_properties_dialog, cx);
            }
            (ContextMenuAction::Properties, ContextMenuTarget::Background { dir_path }) => {
                self.spawn_shell_task(dir_path, show_properties_dialog, cx);
            }
            (ContextMenuAction::RecycleBin, ContextMenuTarget::Item { path, .. }) => {
                self.start_operation(FileOperation::Recycle(path), cx);
            }
            (ContextMenuAction::NewFolder, ContextMenuTarget::Background { dir_path }) => {
                self.open_prompt(
                    PromptKind::NewFolder(dir_path),
                    "New folder",
                    "Enter a name for the folder.",
                    "",
                    "Create",
                    window,
                    cx,
                );
            }
            (ContextMenuAction::Refresh, _) => {
                self.refresh(cx);
            }
            (ContextMenuAction::ShowMoreOptions, target) => {
                let paths = match target {
                    ContextMenuTarget::Item { path, .. } => vec![path],
                    ContextMenuTarget::Background { dir_path } => vec![dir_path],
                };
                self.trigger_native_shell_menu(paths, window, cx);
            }
            _ => {}
        }
    }

    fn trigger_native_shell_menu(
        &mut self,
        paths: Vec<PathBuf>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(hwnd) = extract_window_hwnd(window) else {
            self.status_message = Some("Unable to resolve native window handle.".to_string());
            cx.notify();
            return;
        };

        let affected_dir = self.active_directory();
        let (tx, rx) = oneshot::channel::<Result<(), String>>();

        // `onyx_windows::show_context_menu` requires a fresh STA thread with no prior MTA init.
        let spawn_res = std::thread::Builder::new()
            .name("onyx-shell-context-menu".to_string())
            .spawn(move || {
                let res = show_context_menu(hwnd, &paths).map_err(|err| err.to_string());
                let _ = tx.send(res);
            });

        if let Err(err) = spawn_res {
            self.status_message = Some(format!("Failed to launch native menu: {err}"));
            cx.notify();
            return;
        }

        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |explorer, cx| {
                match result {
                    Ok(()) => {
                        explorer.invalidate_path_caches(&affected_dir);
                        if same_path(&affected_dir, explorer.current_path()) {
                            explorer.load_directory(affected_dir, false, false, cx);
                        } else if explorer.dual_pane
                            && same_path(&affected_dir, &explorer.secondary.path)
                        {
                            explorer.load_secondary_directory(affected_dir, false, cx);
                        }
                    }
                    Err(message) => {
                        explorer.status_message = Some(message);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn handle_watch_event(&mut self, pane: PaneId, changed_path: &Path, cx: &mut Context<Self>) {
        self.invalidate_path_caches(changed_path);
        match pane {
            PaneId::Primary => {
                let current = self.current_path().to_path_buf();
                if (same_path(changed_path, &current) || changed_path.starts_with(&current))
                    && !self.is_loading
                {
                    self.load_directory(current, false, false, cx);
                }
            }
            PaneId::Secondary => {
                if !self.dual_pane {
                    return;
                }
                let current = self.secondary.path.clone();
                if (same_path(changed_path, &current) || changed_path.starts_with(&current))
                    && !self.secondary.is_loading
                {
                    self.load_secondary_directory(current, false, cx);
                }
            }
        }
    }

    // ---- Archive browser & ZIP actions (Stage 3B) ----

    /// Opens the archive browser for `path` and lists its entries asynchronously.
    pub fn open_archive_browser(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.archive_cancel.cancel();
        let cancel = CancellationToken::new();
        self.archive_cancel = cancel.clone();
        self.archive_generation = self.archive_generation.next();
        let generation = self.archive_generation;

        self.command_palette = None;
        self.settings_open = false;
        self.quick_look_open = false;
        self.context_menu = None;
        self.prompt_state = None;
        self.drag = None;
        self.archive = Some(ArchiveViewState::new(path.clone()));
        cx.notify();

        let runtime = Arc::clone(&self.runtime);
        let (tx, rx) = oneshot::channel();
        runtime.spawn(async move {
            let result = list_archive_entries(path, cancel).await;
            let _ = tx.send(result);
        });

        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |explorer, cx| {
                if explorer.archive_generation != generation {
                    return;
                }
                match result {
                    Ok(entries) => {
                        if let Some(archive) = &mut explorer.archive {
                            archive.apply_entries(entries);
                        }
                    }
                    Err(OnyxError::Cancelled) => return,
                    Err(err) => {
                        if let Some(archive) = &mut explorer.archive {
                            archive.apply_error(err.to_string());
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn close_archive(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.archive.take().is_some() {
            self.archive_cancel.cancel();
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    #[must_use]
    pub fn archive_is_open(&self) -> bool {
        self.archive.is_some()
    }

    /// Extracts the open archive into its own folder.
    pub fn extract_archive_here(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.archive.as_ref() else {
            return;
        };
        let archive_path = state.path.clone();
        let Some(parent) = archive_path.parent().map(Path::to_path_buf) else {
            return;
        };
        self.archive = None;
        self.archive_cancel.cancel();
        self.start_operation(
            FileOperation::ExtractArchive {
                archive_path,
                dest_dir: parent,
            },
            cx,
        );
    }

    /// Extracts the open archive into `<parent>/<stem>`.
    pub fn extract_archive_to_folder(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.archive.as_ref() else {
            return;
        };
        let archive_path = state.path.clone();
        let dest_dir = extract_folder_for(&archive_path);
        self.archive = None;
        self.archive_cancel.cancel();
        self.start_operation(
            FileOperation::ExtractArchive {
                archive_path,
                dest_dir,
            },
            cx,
        );
    }

    /// Compresses the selected item into a ZIP next to it.
    pub fn compress_selection_to_zip(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry() else {
            self.status_message = Some("Select a file or folder to compress.".to_string());
            cx.notify();
            return;
        };
        if entry_is_archive(entry) {
            self.status_message = Some("Archives cannot be compressed again.".to_string());
            cx.notify();
            return;
        }
        let sources = vec![entry.path.clone()];
        let current_dir = self.active_directory();
        self.start_operation(
            FileOperation::CreateArchive {
                sources,
                current_dir,
            },
            cx,
        );
    }

    /// Extracts the selected archive into the active directory.
    pub fn extract_selected_archive(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        if !entry_is_archive(entry) {
            self.status_message = Some("Select a .zip, .tar.gz or .tgz archive first.".to_string());
            cx.notify();
            return;
        }
        let archive_path = entry.path.clone();
        let dest_dir = self.active_directory();
        self.start_operation(
            FileOperation::ExtractArchive {
                archive_path,
                dest_dir,
            },
            cx,
        );
    }

    #[must_use]
    pub fn selection_is_archive(&self) -> bool {
        self.selected_entry().is_some_and(entry_is_archive)
    }

    // ---- Internal drag & drop (Stage 3B) ----

    /// Starts a potential drag on mouse-down; it only arms after the threshold.
    pub fn begin_drag(
        &mut self,
        source: PathBuf,
        is_dir: bool,
        origin: (f32, f32),
        cx: &mut Context<Self>,
    ) {
        let label = source.file_name().map_or_else(
            || source.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        self.drag = Some(DragState::new(source, label, is_dir, origin));
        cx.notify();
    }

    /// Tracks the pointer while a drag is in flight.
    pub fn update_drag(&mut self, pointer: (f32, f32), copy: bool, cx: &mut Context<Self>) {
        let Some(drag) = &mut self.drag else {
            return;
        };
        drag.track(pointer, copy);
        if drag.armed {
            cx.notify();
        }
    }

    /// Records the drop target under the pointer while dragging.
    pub fn set_drop_hover(&mut self, target: DropTarget, cx: &mut Context<Self>) {
        let Some(drag) = &mut self.drag else {
            return;
        };
        if !drag.armed || drag.hover.as_ref() == Some(&target) {
            return;
        }
        drag.hover = Some(target);
        cx.notify();
    }

    /// Aborts the drag without touching the file system.
    pub fn cancel_drag(&mut self, cx: &mut Context<Self>) {
        if self.drag.take().is_some() {
            cx.notify();
        }
    }

    /// Completes the drag: copies or moves the source into the hovered folder.
    pub fn finish_drag(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        cx.notify();
        if !drag.armed {
            return;
        }
        let Some(target) = drag.hover else {
            self.status_message = Some("Drop onto a folder, tab, or the other pane.".to_string());
            cx.notify();
            return;
        };
        let destination_dir = match &target {
            DropTarget::Directory(path) => path.clone(),
            DropTarget::Pane(pane) => self.pane_path(*pane).to_path_buf(),
        };
        match validate_drop(&drag.source, &destination_dir) {
            Ok(destination) => {
                let operation = if drag.copy {
                    FileOperation::CopyItems {
                        sources: vec![drag.source],
                        destination,
                    }
                } else {
                    FileOperation::MoveItems {
                        sources: vec![drag.source],
                        destination,
                    }
                };
                self.start_operation(operation, cx);
            }
            Err(reject) => {
                self.status_message = Some(reject.message().to_string());
                cx.notify();
            }
        }
    }

    /// Whether `path` is the folder currently highlighted as a drop target.
    #[must_use]
    pub fn drag_hover_is(&self, path: &Path) -> bool {
        self.drag.as_ref().is_some_and(|drag| {
            drag.armed
                && matches!(
                    drag.hover.as_ref(),
                    Some(DropTarget::Directory(hovered)) if same_path(hovered, path)
                )
        })
    }

    /// Whether the given pane background is the highlighted drop target.
    #[must_use]
    pub fn drag_hover_is_pane(&self, pane: PaneId) -> bool {
        self.drag
            .as_ref()
            .is_some_and(|drag| drag.armed && drag.hover.as_ref() == Some(&DropTarget::Pane(pane)))
    }

    // ---- Command Palette & Settings overlays (Stage 3A) ----

    /// Reports whether a blocking overlay owns the keyboard right now.
    #[must_use]
    pub fn overlay_open(&self) -> bool {
        self.command_palette.is_some() || self.settings_open || self.archive_is_open()
    }

    pub fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.context_menu = None;
        self.prompt_state = None;
        self.settings_open = false;
        self.quick_look_open = false;
        self.palette_input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.focus(window, cx);
        });
        self.command_palette = Some(CommandPaletteState::new(self.palette_input.clone()));
        cx.notify();
    }

    pub fn close_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(mut palette) = self.command_palette.take() {
            // A palette file search dies with the modal that asked for it.
            palette.search.cancel();
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    pub fn toggle_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.command_palette.is_some() {
            self.close_command_palette(window, cx);
        } else {
            self.open_command_palette(window, cx);
        }
    }

    fn on_palette_query_changed(&mut self, query: String, cx: &mut Context<Self>) {
        let mode = {
            let Some(palette) = self.command_palette.as_mut() else {
                return;
            };
            palette.filter.set_query(query);
            palette_mode(&palette.filter.query)
        };
        match mode {
            PaletteMode::Files => {
                let text = self
                    .command_palette
                    .as_ref()
                    .map_or_else(String::new, |palette| {
                        file_search_query(&palette.filter.query).to_string()
                    });
                self.start_palette_search(text, cx);
            }
            PaletteMode::Commands => {
                if let Some(palette) = self.command_palette.as_mut() {
                    palette.search.cancel();
                }
                cx.notify();
            }
        }
    }

    /// Runs the indexed file search of the palette's `?` mode. Every keystroke
    /// restarts the run under a new generation, so stale hits never render.
    fn start_palette_search(&mut self, text: String, cx: &mut Context<Self>) {
        if !self.indexed_search {
            if let Some(palette) = self.command_palette.as_mut() {
                palette.search.fail(
                    "Indexed search is off. Use the `index` command to enable it.".to_string(),
                );
            }
            cx.notify();
            return;
        }
        let Some(palette) = self.command_palette.as_mut() else {
            return;
        };
        if text.trim().is_empty() {
            palette.search.cancel();
            cx.notify();
            return;
        }
        let (generation, cancel) = palette.search.run.restart(text.clone());
        let root = self.current_path().to_path_buf();
        let engine = self.search_engine.clone();
        let query = SearchQuery {
            text,
            include_hidden: self.show_hidden,
            use_index: self.search_index.is_some(),
            ..SearchQuery::default()
        };
        let (tx, rx) = oneshot::channel();
        self.runtime.spawn(async move {
            let result = search_index::run_query(engine, root, query, cancel, true).await;
            let _ = tx.send(result);
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |explorer, cx| {
                explorer.finish_palette_search(generation, result, cx);
            });
        })
        .detach();
        self.ensure_led_animation(cx);
        cx.notify();
    }

    fn finish_palette_search(
        &mut self,
        generation: GenerationId,
        result: OnyxResult<Vec<SearchResult>>,
        cx: &mut Context<Self>,
    ) {
        let show_system = self.config.show_system;
        let Some(palette) = self.command_palette.as_mut() else {
            return;
        };
        if let Some(hits) = palette.search.run.finish(generation, result) {
            palette.search.set_hits(
                hits.into_iter()
                    .filter(|entry| show_system || !entry.attributes.system)
                    .collect(),
            );
        }
        cx.notify();
    }

    fn palette_selected_id(&self) -> Option<&'static str> {
        self.command_palette
            .as_ref()
            .and_then(|palette| palette.filter.selected_id())
    }

    pub fn palette_move_focus(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(palette) = self.command_palette.as_mut() else {
            return;
        };
        if palette_mode(&palette.filter.query) == PaletteMode::Files {
            palette.search.move_focus(delta);
        } else {
            palette.filter.move_focus(delta);
        }
        cx.notify();
    }

    /// Enter in the palette: runs the focused command, or opens the focused hit.
    pub fn activate_palette_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let files_mode = self
            .command_palette
            .as_ref()
            .is_some_and(|palette| palette_mode(&palette.filter.query) == PaletteMode::Files);
        if files_mode {
            let hit = self
                .command_palette
                .as_ref()
                .and_then(|palette| palette.search.selected_hit().cloned());
            self.close_command_palette(window, cx);
            if let Some(entry) = hit {
                self.reveal_entry(entry, window, cx);
            }
        } else if let Some(id) = self.palette_selected_id() {
            self.run_palette_command(id, window, cx);
        }
    }

    /// Closes the palette and runs the command with `id`.
    /// Opens the palette file hit at `index`, used by mouse clicks on a row.
    pub fn open_palette_hit(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(palette) = self.command_palette.as_mut() {
            palette.search.selected = index;
        }
        self.activate_palette_selection(window, cx);
    }

    pub fn run_palette_command(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.close_command_palette(window, cx);
        match id {
            "settings" => self.open_settings(window, cx),
            "new-tab" => self.new_tab(cx),
            "close-tab" => self.close_tab(None, cx),
            "refresh" => self.refresh(cx),
            "up" => self.go_up(cx),
            "favorite" => self.toggle_favorite(cx),
            "quick-look" => self.toggle_quick_look(cx),
            "theme" => self.toggle_theme(window, cx),
            "view-mode" => self.toggle_view_mode(cx),
            "hidden" => self.set_show_hidden(!self.show_hidden, cx),
            "system-files" => self.set_show_system(!self.config.show_system, cx),
            "sidebar" => self.toggle_sidebar(cx),
            "dual" => self.toggle_dual_pane(cx),
            "swap-panes" => self.swap_panes(cx),
            "preview" => self.set_preview_visible(!self.preview_visible, cx),
            "folder-sizes" => self.set_calculate_folder_sizes(!self.calculate_folder_sizes, cx),
            "new-folder" => self.prompt_new_folder(window, cx),
            "rename" => self.prompt_rename(window, cx),
            "duplicate" => self.duplicate_selection(cx),
            "copy" => self.copy_selection(cx),
            "cut" => self.cut_selection(cx),
            "paste" => self.paste(cx),
            "recycle" => self.recycle_selection(cx),
            "copy-path" => self.copy_active_path(cx),
            "properties" => self.show_properties(cx),
            "terminal" => self.open_terminal(cx),
            "index" => self.toggle_indexed_search(cx),
            "index-rebuild" => self.rebuild_index(cx),
            _ => {}
        }
    }

    /// Copies the active directory or the selected item's path to the clipboard.
    pub fn copy_active_path(&mut self, cx: &mut Context<Self>) {
        let path = self
            .selected_entry()
            .map_or_else(|| self.active_directory(), |entry| entry.path.clone());
        cx.write_to_clipboard(ClipboardItem::new_string(normalize_display_path(&path)));
        self.status_message = Some("Copied path to clipboard.".to_string());
        cx.notify();
    }

    pub fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_command_palette(window, cx);
        self.context_menu = None;
        self.prompt_state = None;
        self.quick_look_open = false;
        self.settings_open = true;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    pub fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_open {
            self.settings_open = false;
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    pub fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_open {
            self.close_settings(window, cx);
        } else {
            self.open_settings(window, cx);
        }
    }

    // ---- Settings setters (apply immediately, persist atomically) ----

    pub fn set_theme_mode(&mut self, mode: ThemeMode, window: &mut Window, cx: &mut Context<Self>) {
        self.config.theme = mode;
        self.theme.dark_mode = settings_overlay::resolve_dark_mode(mode);
        self.theme.sync_gpui_theme(Some(window), cx);
        self.persist_config();
        cx.notify();
    }

    pub fn set_default_view(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        self.config.default_view = mode;
        self.view_mode = match mode {
            ViewMode::Grid => ViewMode::Grid,
            ViewMode::Details | ViewMode::List => ViewMode::Details,
        };
        self.persist_config();
        if self.view_mode == ViewMode::Grid {
            self.queue_thumbnails(cx);
        } else {
            self.thumbnail_cancel.cancel();
        }
        cx.notify();
    }

    pub fn set_show_hidden(&mut self, value: bool, cx: &mut Context<Self>) {
        if self.show_hidden == value {
            return;
        }
        self.show_hidden = value;
        self.config.show_hidden = value;
        self.persist_config();
        self.reload_panes_after_listing_change(cx);
        cx.notify();
    }

    pub fn set_show_system(&mut self, value: bool, cx: &mut Context<Self>) {
        if self.config.show_system == value {
            return;
        }
        self.config.show_system = value;
        self.persist_config();
        self.reload_panes_after_listing_change(cx);
        cx.notify();
    }

    pub fn set_preview_visible(&mut self, value: bool, cx: &mut Context<Self>) {
        if self.preview_visible == value {
            return;
        }
        self.preview_visible = value;
        self.config.show_preview_pane = value;
        self.persist_config();
        self.update_preview(cx);
        cx.notify();
    }

    pub fn set_dual_pane_default(&mut self, value: bool, cx: &mut Context<Self>) {
        if self.config.dual_pane_default == value {
            return;
        }
        self.config.dual_pane_default = value;
        self.persist_config();
        cx.notify();
    }

    pub fn set_calculate_folder_sizes(&mut self, value: bool, cx: &mut Context<Self>) {
        if self.calculate_folder_sizes == value {
            return;
        }
        self.calculate_folder_sizes = value;
        self.config.calculate_folder_sizes = value;
        self.persist_config();
        if value {
            self.queue_folder_sizes(cx);
        } else {
            self.folder_size_cancel.cancel();
            self.secondary.folder_size_cancel.cancel();
            self.active_folder_size_tasks = 0;
            self.secondary.active_folder_size_tasks = 0;
        }
        cx.notify();
    }

    pub fn set_default_terminal(&mut self, value: &str, cx: &mut Context<Self>) {
        self.config.set_default_terminal(value);
        self.persist_config();
        cx.notify();
    }

    pub fn add_excluded_index_dir(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.config.add_excluded_index_dir(path) {
            self.persist_config();
            self.restart_indexer(cx);
            cx.notify();
        }
    }

    pub fn remove_excluded_index_dir(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.config.remove_excluded_index_dir(path) {
            self.persist_config();
            self.restart_indexer(cx);
            cx.notify();
        }
    }

    pub fn clear_excluded_index_dirs(&mut self, cx: &mut Context<Self>) {
        if self.config.excluded_index_dirs.is_empty() {
            return;
        }
        self.config.excluded_index_dirs.clear();
        self.persist_config();
        self.restart_indexer(cx);
        cx.notify();
    }

    /// Re-opens the index worker with the current exclusions and rebuilds the
    /// open folder, so new exclusion rules apply to the watcher and the index.
    fn restart_indexer(&mut self, cx: &mut Context<Self>) {
        if !self.indexed_search {
            return;
        }
        self.index.disable();
        self.search_index = None;
        self.search_engine = UnifiedSearchEngine::live_only();
        self.index_status = IndexStatus::Opening;
        self.index
            .enable(self.index_roots(), self.config.excluded_index_dirs.clone());
        self.request_index_build(self.current_path().to_path_buf(), cx);
        cx.notify();
    }

    fn reload_panes_after_listing_change(&mut self, cx: &mut Context<Self>) {
        let path = self.current_path().to_path_buf();
        self.directories.invalidate(&path);
        self.load_directory(path, false, false, cx);
        if self.dual_pane {
            let secondary = self.secondary.path.clone();
            self.directories.invalidate(&secondary);
            self.load_secondary_directory(secondary, false, cx);
        }
    }

    // ---- Global Keyboard Shortcuts ----

    fn handle_key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = ev.keystroke.key.as_str();
        let mods = &ev.keystroke.modifiers;

        if let Some(menu) = &mut self.context_menu {
            match key {
                "escape" => {
                    self.close_context_menu(cx);
                    cx.stop_propagation();
                    return;
                }
                "down" => {
                    menu.move_focus(1);
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                "up" => {
                    menu.move_focus(-1);
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                "enter" | "space" => {
                    if let Some(action) = menu.focused_action() {
                        self.execute_context_menu_action(action, window, cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }

        if self.prompt_state.is_some() {
            if key == "escape" {
                self.cancel_prompt(window, cx);
                cx.stop_propagation();
            }
            return;
        }

        // The command palette stays modal: it owns the keyboard while open and
        // routes Escape/arrows/Enter itself.
        if self.command_palette.is_some() {
            match key {
                "escape" => {
                    self.close_command_palette(window, cx);
                    cx.stop_propagation();
                    return;
                }
                "up" => {
                    self.palette_move_focus(-1, cx);
                    cx.stop_propagation();
                    return;
                }
                "down" => {
                    self.palette_move_focus(1, cx);
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
            if mods.control && key == "k" {
                self.close_command_palette(window, cx);
                cx.stop_propagation();
            }
            return;
        }

        if self.archive.is_some() {
            if key == "escape" {
                self.close_archive(window, cx);
                cx.stop_propagation();
            }
            return;
        }

        if self.drag.is_some() && key == "escape" {
            self.cancel_drag(cx);
            cx.stop_propagation();
            return;
        }

        if self.settings_open {
            if key == "escape" || (mods.control && key == ",") {
                self.close_settings(window, cx);
                cx.stop_propagation();
            } else if mods.control && key == "k" {
                self.close_settings(window, cx);
                self.open_command_palette(window, cx);
                cx.stop_propagation();
            }
            return;
        }

        if mods.control && key == "k" {
            self.toggle_command_palette(window, cx);
            cx.stop_propagation();
            return;
        }
        if mods.control && key == "," {
            self.toggle_settings(window, cx);
            cx.stop_propagation();
            return;
        }

        // A live splitter drag swallows Escape so the resized boundary snaps back.
        // Any other key reports the drag as finished first, so a release outside the
        // window can never leave the splitter stuck and keyboard input dead.
        if self.splitter_drag.is_some() {
            if key == "escape" {
                self.cancel_splitter_drag(cx);
                cx.stop_propagation();
                return;
            }
            self.end_splitter_drag(cx);
        }

        if mods.control && key == "l" {
            self.begin_address_edit(window, cx);
            cx.stop_propagation();
            return;
        }
        if mods.control && key == "f" {
            self.search_input
                .update(cx, |input, cx| input.focus(window, cx));
            cx.stop_propagation();
            return;
        }
        if mods.control && key == "t" {
            self.new_tab(cx);
            cx.stop_propagation();
            return;
        }
        if mods.control && key == "w" {
            self.close_tab(None, cx);
            cx.stop_propagation();
            return;
        }
        if mods.control && !mods.shift && key == "b" {
            self.toggle_sidebar(cx);
            cx.stop_propagation();
            return;
        }
        if mods.alt && key == "p" {
            self.toggle_preview_pane(cx);
            cx.stop_propagation();
            return;
        }
        if mods.control && key == "`" {
            self.open_terminal(cx);
            cx.stop_propagation();
            return;
        }
        if mods.alt && key == "enter" {
            self.show_properties(cx);
            cx.stop_propagation();
            return;
        }
        if key == "f5" {
            self.refresh(cx);
            cx.stop_propagation();
            return;
        }
        if mods.alt && key == "left" {
            self.go_back(cx);
            cx.stop_propagation();
            return;
        }
        if mods.alt && key == "right" {
            self.go_forward(cx);
            cx.stop_propagation();
            return;
        }
        if mods.alt && key == "up" {
            self.go_up(cx);
            cx.stop_propagation();
            return;
        }

        // Ignore file-pane navigation & file-operation keys while a text input is focused
        let search_focused = self
            .search_input
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        if self.address_editing || search_focused {
            return;
        }

        // Quick Look modal captures Space, Escape, Enter, and Arrow keys
        if self.quick_look_open {
            match key {
                "escape" | "space" | " " => {
                    self.close_quick_look(cx);
                    cx.stop_propagation();
                }
                "left" | "up" => {
                    self.move_selection(-1, cx);
                    cx.stop_propagation();
                }
                "right" | "down" => {
                    self.move_selection(1, cx);
                    cx.stop_propagation();
                }
                "enter" => {
                    self.open_selected_from_quick_look(cx);
                    cx.stop_propagation();
                }
                _ => {}
            }
            return;
        }

        if !mods.control && !mods.alt && (key == "space" || key == " ") {
            self.toggle_quick_look(cx);
            cx.stop_propagation();
            return;
        }

        if key == "f2" {
            self.prompt_rename(window, cx);
            cx.stop_propagation();
            return;
        }
        if mods.shift && key == "delete" {
            self.prompt_permanent_delete(window, cx);
            cx.stop_propagation();
            return;
        }
        if !mods.shift && !mods.control && !mods.alt && key == "delete" {
            self.recycle_selection(cx);
            cx.stop_propagation();
            return;
        }
        if mods.control && !mods.shift && !mods.alt && key == "c" {
            self.copy_selection(cx);
            cx.stop_propagation();
            return;
        }
        if mods.control && !mods.shift && !mods.alt && key == "x" {
            self.cut_selection(cx);
            cx.stop_propagation();
            return;
        }
        if mods.control && !mods.shift && !mods.alt && key == "v" {
            self.paste(cx);
            cx.stop_propagation();
            return;
        }
        if mods.control && !mods.shift && !mods.alt && key == "z" {
            self.undo(cx);
            cx.stop_propagation();
            return;
        }
        if mods.control && mods.shift && key == "s" {
            self.swap_panes(cx);
            cx.stop_propagation();
            return;
        }
        if mods.control && mods.shift && key == "n" {
            self.prompt_new_folder(window, cx);
            cx.stop_propagation();
            return;
        }

        if self.view_mode == ViewMode::Grid {
            let viewport_w: f32 = window.viewport_size().width.into();
            let layout = splitter::compute_workspace_layout(
                viewport_w,
                self.sidebar_visible,
                self.sidebar_width,
                self.dual_pane,
                self.pane_split_ratio,
                self.preview_visible,
                self.preview_width,
            );
            let pane_w = match self.active_pane {
                PaneId::Primary => layout.primary_w,
                PaneId::Secondary => layout.secondary_w,
            };
            let cols = file_pane::compute_grid_columns(pane_w.max(240.0)) as isize;
            match key {
                "left" => self.move_selection(-1, cx),
                "right" => self.move_selection(1, cx),
                "up" => self.move_selection(-cols, cx),
                "down" => self.move_selection(cols, cx),
                "enter" => {
                    if let Some(idx) = self.pane_selected_index(self.active_pane) {
                        self.activate_entry(idx, cx);
                    }
                }
                "backspace" => self.go_up(cx),
                _ => {}
            }
        } else {
            match key {
                "up" => self.move_selection(-1, cx),
                "down" => self.move_selection(1, cx),
                "enter" => {
                    if let Some(idx) = self.pane_selected_index(self.active_pane) {
                        self.activate_entry(idx, cx);
                    }
                }
                "backspace" => self.go_up(cx),
                _ => {}
            }
        }
    }
}

impl Render for OnyxExplorer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let viewport = window.viewport_size();
        self.viewport_size = (f32::from(viewport.width), f32::from(viewport.height));

        if self.list_scroll_animating() {
            self.arm_scroll_frame(window, cx);
        }

        let layout = splitter::compute_workspace_layout(
            self.viewport_size.0,
            self.sidebar_visible,
            self.sidebar_width,
            self.dual_pane,
            self.pane_split_ratio,
            self.preview_visible,
            self.preview_width,
        );
        let dragging_kind = self.splitter_drag.map(|drag| drag.kind);

        let mut workspace = div().flex().flex_row().flex_1().min_h(px(0.0)).w_full();

        if let Some(sidebar_el) = sidebar::render_sidebar(self, layout.sidebar_w, cx) {
            workspace = workspace.child(sidebar_el);
        }
        if self.sidebar_visible {
            workspace = workspace.child(splitter::render_splitter(
                SplitterKind::Sidebar,
                dragging_kind == Some(SplitterKind::Sidebar),
                theme,
                cx,
            ));
        }
        workspace = workspace.child(file_pane::render_file_pane(
            self,
            PaneId::Primary,
            layout.primary_w,
            cx,
        ));
        if self.dual_pane {
            workspace = workspace.child(splitter::render_splitter(
                SplitterKind::DualPane,
                dragging_kind == Some(SplitterKind::DualPane),
                theme,
                cx,
            ));
            workspace = workspace.child(file_pane::render_file_pane(
                self,
                PaneId::Secondary,
                layout.secondary_w,
                cx,
            ));
        }
        if self.preview_visible {
            workspace = workspace.child(splitter::render_splitter(
                SplitterKind::Preview,
                dragging_kind == Some(SplitterKind::Preview),
                theme,
                cx,
            ));
        }
        if let Some(preview_el) = preview_pane::render_preview_pane(self, layout.preview_w, cx) {
            workspace = workspace.child(preview_el);
        }

        let mut root = div()
            .id("onyx-root")
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(theme.bg_base())
            .text_color(theme.text_primary())
            .on_key_down(cx.listener(|explorer, ev: &KeyDownEvent, window, cx| {
                explorer.handle_key_down(ev, window, cx);
            }))
            .on_mouse_move(cx.listener(|explorer, ev: &MouseMoveEvent, _, cx| {
                let pointer = (f32::from(ev.position.x), f32::from(ev.position.y));
                if explorer.splitter_drag.is_some() {
                    if ev.pressed_button == Some(MouseButton::Left) {
                        explorer.update_splitter_drag(pointer.0, cx);
                    } else {
                        explorer.end_splitter_drag(cx);
                    }
                } else if explorer.drag.is_some() {
                    if ev.pressed_button == Some(MouseButton::Left) {
                        explorer.update_drag(pointer, ev.modifiers.control, cx);
                    } else {
                        explorer.cancel_drag(cx);
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|explorer, _: &MouseUpEvent, _, cx| {
                    explorer.end_splitter_drag(cx);
                    explorer.finish_drag(cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|explorer, _: &MouseUpEvent, _, cx| {
                    explorer.end_splitter_drag(cx);
                    explorer.finish_drag(cx);
                }),
            )
            .child(tab_bar::render_tab_bar(self, window, cx))
            .child(nav_bar::render_nav_bar(self, window, cx))
            .child(toolbar::render_toolbar(self, window, cx))
            .child(workspace)
            .child(status_bar::render_status_bar(self, cx));

        if let Some(suggestions_overlay) = nav_bar::render_address_suggestions_overlay(self, cx) {
            root = root.child(suggestions_overlay);
        }
        if let Some(quick_look_overlay) = preview_pane::render_quick_look_modal(self, cx) {
            root = root.child(quick_look_overlay);
        }
        if let Some(progress_toast) = overlays::render_operation_progress_toast(self, cx) {
            root = root.child(progress_toast);
        }
        if let Some(prompt_dialog) = overlays::render_prompt_dialog(self, cx) {
            root = root.child(prompt_dialog);
        }
        if let Some(context_overlay) = context_menu::render_context_menu_overlay(self, window, cx) {
            root = root.child(context_overlay);
        }
        if let Some(palette_overlay) = command_palette::render_command_palette(self, cx) {
            root = root.child(palette_overlay);
        }
        if let Some(archive_element) = archive_overlay::render_archive_overlay(self, window, cx) {
            root = root.child(archive_element);
        }
        if let Some(settings_element) = settings_overlay::render_settings_overlay(self, window, cx)
        {
            root = root.child(settings_element);
        }
        if let Some(ghost) = drag_drop::render_drag_ghost(self) {
            root = root.child(ghost);
        }

        root
    }
}

fn spawn_directory_watcher(
    runtime: Arc<Runtime>,
    shutdown: CancellationToken,
    pane: PaneId,
    cx: &mut Context<OnyxExplorer>,
) -> mpsc::UnboundedSender<PathBuf> {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<PathBuf>();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<WatchEvent>();
    let handle = runtime.handle().clone();

    runtime.spawn_blocking(move || {
        let Ok(mut watcher) = DirectoryWatcher::new(event_tx) else {
            return;
        };

        loop {
            let next_path = handle.block_on(async {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => None,
                    path = cmd_rx.recv() => path,
                }
            });
            let Some(mut latest) = next_path else {
                break;
            };
            // Coalesce rapid navigation requests so only the latest directory is watched.
            while let Ok(newer) = cmd_rx.try_recv() {
                latest = newer;
            }
            let _ = watcher.switch_path(&latest);
        }
    });

    cx.spawn(async move |this, cx| {
        while let Some(event) = event_rx.recv().await {
            let ok = this
                .update(cx, |explorer, cx| {
                    explorer.handle_watch_event(pane, &event.path, cx);
                })
                .is_ok();
            if !ok {
                break;
            }
        }
    })
    .detach();

    cmd_tx
}

fn extract_window_hwnd(window: &Window) -> Option<isize> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(win32) => Some(win32.hwnd.get()),
        _ => None,
    }
}

pub(crate) fn same_path(first: &Path, second: &Path) -> bool {
    let normalize = |path: &Path| {
        normalize_display_path(path)
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    first == second || normalize(first) == normalize(second)
}

/// Filters `all_entries` by `needle`, overlays cached folder sizes and sorts the
/// result for one pane.
fn build_view_entries(
    all_entries: &[FileEntry],
    sort_spec: SortSpec,
    needle: &str,
    folder_sizes: &FolderSizeCache,
) -> Vec<FileEntry> {
    let mut filtered: Vec<FileEntry> = if needle.is_empty() {
        all_entries.to_vec()
    } else {
        all_entries
            .iter()
            .filter(|entry| {
                entry.name.to_ascii_lowercase().contains(needle)
                    || entry.type_label.to_ascii_lowercase().contains(needle)
            })
            .cloned()
            .collect()
    };
    for entry in &mut filtered {
        if entry.is_dir()
            && let Some(cached) = folder_sizes.get(&entry.path)
        {
            entry.size = cached.total_bytes;
        }
    }
    sort_entries(&mut filtered, sort_spec);
    filtered
}

fn compute_size_ratio(known_bytes: Option<u64>, max_bytes: u64) -> f32 {
    match known_bytes {
        Some(bytes) if max_bytes > 0 && bytes > 0 => {
            ((bytes as f64) / (max_bytes as f64)).clamp(0.0, 1.0) as f32
        }
        _ => 0.0,
    }
}

/// Lists up to 8 subdirectories of `parent` whose names case-insensitively
/// start with `prefix`, sorted naturally. Runs on a blocking worker only.
fn list_address_suggestions(parent: &Path, prefix: &str) -> Vec<String> {
    let prefix = prefix.to_ascii_lowercase();
    let mut matches: Vec<(String, String)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.to_ascii_lowercase().starts_with(&prefix) {
            continue;
        }
        let path = normalize_display_path(&entry.path());
        matches.push((name, path));
    }
    matches.sort_by(|first, second| natural_cmp(&first.0, &second.0));
    matches.truncate(8);
    matches.into_iter().map(|(_, path)| path).collect()
}

/// Whether the entry is a supported archive (`.zip`, `.tar.gz`, `.tgz`).
fn entry_is_archive(entry: &FileEntry) -> bool {
    !entry.is_dir() && is_supported_archive_path(&entry.path)
}

/// Computes `<dir>/<stem>.zip` for `sources`, appending ` (n)` until `exists`
/// reports the path as free.
fn archive_destination(sources: &[PathBuf], dir: &Path, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let base = if sources.len() == 1 {
        let only = &sources[0];
        let stem = if only.is_dir() {
            only.file_name()
        } else {
            only.file_stem().or_else(|| only.file_name())
        };
        stem.and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("Archive")
            .to_string()
    } else {
        "Archive".to_string()
    };

    let first = dir.join(format!("{base}.zip"));
    if !exists(&first) {
        return first;
    }
    for index in 2..=999 {
        let candidate = dir.join(format!("{base} ({index}).zip"));
        if !exists(&candidate) {
            return candidate;
        }
    }
    first
}

/// Extraction folder used by "Extract to Folder": `<parent>/<stem>`.
fn extract_folder_for(archive_path: &Path) -> PathBuf {
    let parent = archive_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stem = archive_path
        .file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("Extracted");
    parent.join(stem)
}

fn add_parent(affected: &mut Vec<PathBuf>, path: &Path) {
    if let Some(parent) = path.parent()
        && !affected.iter().any(|existing| existing == parent)
    {
        affected.push(parent.to_path_buf());
    }
}

fn terminal_profile_from_config(value: &str) -> TerminalProfile {
    match value.trim().to_ascii_lowercase().as_str() {
        "pwsh" | "powershell" => TerminalProfile::PowerShell,
        "cmd" | "command-prompt" => TerminalProfile::Cmd,
        _ => TerminalProfile::WindowsTerminal,
    }
}

fn progress_fraction(progress: &OperationProgress) -> f32 {
    let fraction = if progress.total_bytes > 0 {
        progress.bytes_copied as f32 / progress.total_bytes as f32
    } else if progress.total_items > 0 {
        progress.completed_items as f32 / progress.total_items as f32
    } else {
        0.0
    };
    fraction.clamp(0.0, 1.0)
}

fn operation_message(title: &str, result: OnyxResult<Option<OperationReport>>) -> String {
    match result {
        Ok(None) => "Nothing to undo.".into(),
        Ok(Some(report)) if report.failed.is_empty() && report.skipped.is_empty() => {
            format!("{title} complete · {} items", report.succeeded.len())
        }
        Ok(Some(report)) => {
            let summary = format!(
                "{title} · {} completed, {} skipped, {} failed",
                report.succeeded.len(),
                report.skipped.len(),
                report.failed.len()
            );
            report.failed.first().map_or_else(
                || summary.clone(),
                |(path, error)| format!("{summary} · {}: {error}", path.display()),
            )
        }
        Err(OnyxError::Cancelled) => format!("{title} cancelled."),
        Err(error) => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onyx_core::{FileAttributes, FileKind};

    fn test_entry(name: &str, kind: FileKind, size: u64) -> FileEntry {
        FileEntry {
            path: PathBuf::from(format!("C:\\test\\{name}")),
            name: name.to_string(),
            extension: None,
            kind,
            size,
            modified: None,
            created: None,
            attributes: FileAttributes::default(),
            is_symlink: false,
            symlink_target: None,
            type_label: "File".to_string(),
        }
    }

    #[test]
    fn archive_destination_renames_when_occupied() {
        let sources = vec![PathBuf::from("C:/data/photos")];
        let dir = Path::new("C:/data");
        assert_eq!(
            archive_destination(&sources, dir, |_| false),
            PathBuf::from("C:/data/photos.zip")
        );
        assert_eq!(
            archive_destination(&sources, dir, |candidate| {
                matches!(
                    candidate.file_name().and_then(|name| name.to_str()),
                    Some("photos.zip" | "photos (2).zip" | "photos (3).zip")
                )
            }),
            PathBuf::from("C:/data/photos (4).zip")
        );
        let taken = PathBuf::from("C:/data/photos.zip");
        assert_eq!(
            archive_destination(&sources, dir, move |candidate| candidate == taken),
            PathBuf::from("C:/data/photos (2).zip")
        );

        let many = vec![
            PathBuf::from("C:/data/a.txt"),
            PathBuf::from("C:/data/b.txt"),
        ];
        assert_eq!(
            archive_destination(&many, dir, |_| false),
            PathBuf::from("C:/data/Archive.zip")
        );
    }

    #[test]
    fn extract_folder_uses_archive_stem_next_to_the_file() {
        assert_eq!(
            extract_folder_for(Path::new("C:/data/photos.zip")),
            PathBuf::from("C:/data/photos")
        );
        assert_eq!(
            extract_folder_for(Path::new("photos.zip")),
            PathBuf::from("./photos")
        );
    }

    #[test]
    fn archive_detection_covers_supported_extensions() {
        let zip = test_entry("a.zip", FileKind::File, 1);
        let tar = test_entry("b.tar.gz", FileKind::File, 1);
        let txt = test_entry("c.txt", FileKind::File, 1);
        let dir = test_entry("d.zip", FileKind::Directory, 1);
        assert!(entry_is_archive(&zip));
        assert!(entry_is_archive(&tar));
        assert!(!entry_is_archive(&txt));
        assert!(!entry_is_archive(&dir));
    }

    #[test]
    fn view_entries_filter_and_overlay_cached_folder_sizes() {
        let sizes = FolderSizeCache::default();
        sizes.put(
            PathBuf::from("C:\\test\\dir"),
            CachedFolderSize::new(4096, 3, false),
        );
        let all_entries = vec![
            test_entry("dir", FileKind::Directory, 0),
            test_entry("report.txt", FileKind::File, 128),
        ];

        let filtered = build_view_entries(&all_entries, SortSpec::default(), "rep", &sizes);
        assert_eq!(filtered.len(), 1);
        assert_eq!(
            filtered.first().map(|entry| entry.name.as_str()),
            Some("report.txt")
        );

        let full = build_view_entries(&all_entries, SortSpec::default(), "", &sizes);
        assert_eq!(full.len(), 2);
        assert_eq!(
            full.iter()
                .find(|entry| entry.name == "dir")
                .map(|e| e.size),
            Some(4096)
        );
        assert_eq!(
            full.iter()
                .find(|entry| entry.name == "report.txt")
                .map(|e| e.size),
            Some(128)
        );
    }

    #[test]
    fn size_ratio_bounds_match_slint_ui() {
        assert_eq!(compute_size_ratio(None, 1000), 0.0);
        assert_eq!(compute_size_ratio(Some(0), 1000), 0.0);
        assert_eq!(compute_size_ratio(Some(500), 0), 0.0);
        assert!((compute_size_ratio(Some(250), 1000) - 0.25).abs() < 1e-6);
        assert!((compute_size_ratio(Some(2000), 1000) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn grid_columns_respect_minimum_width() {
        assert_eq!(file_pane::compute_grid_columns(100.0), 1);
        assert_eq!(file_pane::compute_grid_columns(444.0), 3);
    }

    #[test]
    fn terminal_profile_mapping_covers_config_values() {
        assert_eq!(
            terminal_profile_from_config("wt"),
            TerminalProfile::WindowsTerminal
        );
        assert_eq!(
            terminal_profile_from_config("pwsh"),
            TerminalProfile::PowerShell
        );
        assert_eq!(terminal_profile_from_config("cmd"), TerminalProfile::Cmd);
        assert_eq!(
            terminal_profile_from_config("unknown"),
            TerminalProfile::WindowsTerminal
        );
    }

    #[test]
    fn operation_message_formats_outcomes() {
        assert_eq!(operation_message("Undo", Ok(None)), "Nothing to undo.");
        assert_eq!(
            operation_message("Copy", Err(OnyxError::Cancelled)),
            "Copy cancelled."
        );
        let mut report = OperationReport::default();
        report.succeeded.push(PathBuf::from("a.txt"));
        assert_eq!(
            operation_message("Copy", Ok(Some(report))),
            "Copy complete · 1 items"
        );
    }
}
