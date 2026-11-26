//! Primary Onyx GPUI explorer coordinator wired to `onyx-core`, `onyx-fs`,
//! `onyx-cache`, `onyx-config`, and `onyx-windows`.

use crate::components::a11y::{self, ModalSurface, Modals};
use crate::components::archive_overlay::{self, ArchiveViewState};
use crate::components::command_palette::{
    self, CommandPaletteState, PaletteMode, file_search_query, palette_mode,
};
use crate::components::context_menu::{ContextMenuAction, ContextMenuState, ContextMenuTarget};
use crate::components::drag_drop::{self, DragState, DropTarget, validate_drop};
use crate::components::overlays::{
    self, ConflictDialogState, ConflictOutcome, ResolvedTransfer, TransferKind,
    conflict_resolution_from_policy, detect_conflicts, operation_kind_title,
};
use crate::components::preview_pane::{DecodedPreview, PreviewKind, PreviewState};
use crate::components::settings_overlay;
use crate::components::smooth_scroll::{
    DETAILS_ROW_HEIGHT, SmoothScroll, content_height, keep_visible, wheel_pixels,
};
use crate::components::splitter::{self, ActiveSplitterDrag, SplitterKind};
use crate::components::{
    context_menu, file_pane, nav_bar, preview_pane, sidebar, status_bar, tab_bar, toolbar,
};
use crate::icons::{
    RECYCLE_BIN_ICON_PREFIX, ShellIconStore, entry_icon_key, path_icon_key,
    rgba_pixels_to_render_image,
};
use crate::search_index::{
    self, IndexController, IndexEvent, IndexStatus, ListMode, SearchRun, SearchStatus,
    SearchStreamEvent,
};
use crate::theme::OnyxTheme;
use gpui_kit::{
    AppContext, ClipboardItem, Context, DispatchPhase, Entity, ExternalPaths, FileDropEvent,
    FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, NavigationDirection, ParentElement, Pixels,
    Point, Render, RenderImage, Role, ScrollEvent, ScrollHandle, StatefulInteractiveElement,
    Styled, Subscription, UniformListScrollHandle, Window, canvas,
    component::input::{InputEvent, InputState},
    div, point, px,
};
use onyx_cache::{
    CachedFolderSize, DirectoryCache, FolderSizeCache, FolderSizeStore, dir_mtime_secs,
};
use onyx_config::{AppConfig, ConfigStore, PreviewSide, SessionState};
use onyx_core::{
    DriveInfo, FavoriteItem, FileAttributes, FileEntry, FileKind, GenerationId, GroupByMode,
    KnownFolder, NavHistory, OnyxError, OnyxResult, SortColumn, SortDirection, SortSpec,
    TabManager, TabState, ThemeMode, ViewMode, expand_env_vars, format_bytes, fuzzy_match,
    group_entries, natural_cmp, normalize_display_path, sort_entries, split_autocomplete_prefix,
};
use onyx_fs::{
    DEFAULT_MAX_FOLDER_SIZE_ENTRIES, DirectoryWatcher, EnumerationOptions, FolderSizeResult,
    FsEngine, WatchEvent, compute_folder_size,
};
use onyx_operations::{
    ConflictDecision, ConflictInfo, ConflictPolicy, ConflictResolution, FileOperationsEngine,
    OperationKind, OperationProgress, OperationReport, PermanentDeleteRequest, UndoAction,
    create_archive, extract_archive, is_supported_archive_path, list_archive_entries,
};
use onyx_preview::{PreviewEngine, ThumbnailEngine, is_thumbnail_supported};
use onyx_search::{SearchIndex, SearchQuery, UnifiedSearchEngine};
use onyx_windows::{
    DragImageSource, DragOutcome, RecycleBinItem, RgbaIcon, TerminalProfile, empty_recycle_bin,
    enumerate_drives, enumerate_known_folders, enumerate_recycle_bin, extract_file_icon_rgba,
    extract_path_icon_rgba, get_clipboard_files, launch_terminal_with_profile, open_file,
    open_with_dialog, recycle_bin_icon_rgba, restore_recycle_bin_items, set_clipboard_files,
    show_context_menu, show_properties_dialog, start_external_file_drag,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PromptKind {
    NewFolder(PathBuf),
    Rename(PathBuf),
    PermanentDelete(PathBuf),
    EmptyRecycleBin,
}

impl PromptKind {
    /// Whether this prompt is a destructive confirmation without a text field.
    const fn is_danger(&self) -> bool {
        matches!(self, Self::PermanentDelete(_) | Self::EmptyRecycleBin)
    }
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

/// Files another application drags into the window.
#[derive(Debug, Clone)]
struct ExternalDropIn {
    paths: Vec<PathBuf>,
    position: (f32, f32),
}

/// Live state for the bottom-right floating Operation Progress card.
#[derive(Debug, Clone)]
pub(crate) struct ActiveOperation {
    pub title: &'static str,
    pub kind: Option<OperationKind>,
    pub current_item: String,
    pub completed_items: usize,
    pub total_items: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub error: Option<String>,
    pub cancelling: bool,
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
    Recycle(PathBuf),
    PermanentDelete(PermanentDeleteRequest),
    CopyItems {
        sources: Vec<PathBuf>,
        destination: PathBuf,
        conflict: ConflictPolicy,
        per_destination: HashMap<PathBuf, ConflictResolution>,
    },
    MoveItems {
        sources: Vec<PathBuf>,
        destination: PathBuf,
        conflict: ConflictPolicy,
        per_destination: HashMap<PathBuf, ConflictResolution>,
        from_cut_clipboard: bool,
    },
    CreateArchive {
        sources: Vec<PathBuf>,
        current_dir: PathBuf,
    },
    ExtractArchive {
        archive_path: PathBuf,
        dest_dir: PathBuf,
    },
    RestoreRecycleItems(Vec<RecycleBinItem>),
    EmptyRecycleBin,
    Undo,
}

impl FileOperation {
    const fn title(&self) -> &'static str {
        match self {
            Self::Create { .. } => "New folder",
            Self::Rename { .. } => "Rename",
            Self::Duplicate(_) => "Duplicate",
            Self::Recycle(_) => "Move to Recycle Bin",
            Self::PermanentDelete(_) => "Permanent delete",
            Self::CopyItems { .. } => "Copy",
            Self::MoveItems { .. } => "Move",
            Self::CreateArchive { .. } => "Compress to ZIP",
            Self::ExtractArchive { .. } => "Extract ZIP",
            Self::RestoreRecycleItems(_) => "Restore",
            Self::EmptyRecycleBin => "Empty Recycle Bin",
            Self::Undo => "Undo",
        }
    }

    const fn progress_title(&self) -> &'static str {
        match self {
            Self::Create { .. } => "Creating folder",
            Self::Rename { .. } => "Renaming",
            Self::Duplicate(_) => "Duplicating",
            Self::Recycle(_) => "Recycling",
            Self::PermanentDelete(_) => "Deleting",
            Self::CopyItems { .. } => "Copying",
            Self::MoveItems { .. } => "Moving",
            Self::CreateArchive { .. } => "Compressing",
            Self::ExtractArchive { .. } => "Extracting",
            Self::RestoreRecycleItems(_) => "Restoring",
            Self::EmptyRecycleBin => "Emptying Recycle Bin",
            Self::Undo => "Undoing",
        }
    }

    const fn operation_kind(&self) -> Option<OperationKind> {
        match self {
            Self::Create { .. } => Some(OperationKind::CreateDir),
            Self::Rename { .. } => Some(OperationKind::Rename),
            Self::Duplicate(_) => Some(OperationKind::Duplicate),
            Self::Recycle(_) => Some(OperationKind::Recycle),
            Self::PermanentDelete(_) => Some(OperationKind::PermanentDelete),
            Self::CopyItems { .. } => Some(OperationKind::Copy),
            Self::MoveItems { .. } => Some(OperationKind::Move),
            Self::CreateArchive { .. } => Some(OperationKind::CreateArchive),
            Self::ExtractArchive { .. } => Some(OperationKind::ExtractArchive),
            Self::RestoreRecycleItems(_) | Self::EmptyRecycleBin | Self::Undo => None,
        }
    }

    const fn is_recycle_bin_operation(&self) -> bool {
        matches!(self, Self::RestoreRecycleItems(_) | Self::EmptyRecycleBin)
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
                conflict,
                per_destination,
            } => {
                affected.push(destination.clone());
                for path in &sources {
                    add_parent(affected, path);
                }
                if per_destination.is_empty() {
                    engine
                        .copy_items(&sources, &destination, conflict, cancel, Some(progress))
                        .await
                } else {
                    let fallback = conflict_resolution_from_policy(conflict);
                    engine
                        .copy_items_with_resolver(
                            &sources,
                            &destination,
                            conflict,
                            move |info: &ConflictInfo| {
                                let resolution = per_destination
                                    .iter()
                                    .find(|(path, _)| same_path(path, &info.destination))
                                    .map_or(fallback, |(_, &res)| res);
                                ConflictDecision::once(resolution)
                            },
                            cancel,
                            Some(progress),
                        )
                        .await
                }
            }
            Self::MoveItems {
                sources,
                destination,
                conflict,
                per_destination,
                from_cut_clipboard,
            } => {
                affected.push(destination.clone());
                for path in &sources {
                    add_parent(affected, path);
                }
                let report = if per_destination.is_empty() {
                    engine
                        .move_items(&sources, &destination, conflict, cancel, Some(progress))
                        .await
                } else {
                    let fallback = conflict_resolution_from_policy(conflict);
                    engine
                        .move_items_with_resolver(
                            &sources,
                            &destination,
                            conflict,
                            move |info: &ConflictInfo| {
                                let resolution = per_destination
                                    .iter()
                                    .find(|(path, _)| same_path(path, &info.destination))
                                    .map_or(fallback, |(_, &res)| res);
                                ConflictDecision::once(resolution)
                            },
                            cancel,
                            Some(progress),
                        )
                        .await
                };
                if from_cut_clipboard
                    && let Ok(report) = &report
                    && let Some(UndoAction::ReverseMoves(moves)) = &report.undo_action
                {
                    let moved = moves.iter().map(|(_, source)| source.clone()).collect();
                    let _ = clipboard.send(ClipboardCommand::Moved {
                        original: sources,
                        moved,
                    });
                }
                report
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
            Self::RestoreRecycleItems(items) => {
                let mut restored_paths = Vec::with_capacity(items.len());
                for item in &items {
                    if let Some(original) = &item.original_path {
                        add_parent(affected, original);
                        restored_paths.push(original.clone());
                    } else {
                        restored_paths.push(item.shell_path.clone());
                    }
                }
                let cancel_for_restore = cancel.clone();
                tokio::task::spawn_blocking(move || {
                    if cancel_for_restore.is_cancelled() {
                        return Err(OnyxError::Cancelled);
                    }
                    restore_recycle_bin_items(&items)
                })
                .await
                .map_err(|err| OnyxError::OperationFailed {
                    message: format!("Recycle Bin restore task join failed: {err}"),
                })??;
                Ok(OperationReport {
                    succeeded: restored_paths,
                    ..OperationReport::default()
                })
            }
            Self::EmptyRecycleBin => {
                let cancel_for_empty = cancel.clone();
                tokio::task::spawn_blocking(move || {
                    if cancel_for_empty.is_cancelled() {
                        return Err(OnyxError::Cancelled);
                    }
                    empty_recycle_bin()
                })
                .await
                .map_err(|err| OnyxError::OperationFailed {
                    message: format!("Empty Recycle Bin task join failed: {err}"),
                })??;
                Ok(OperationReport::default())
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
        is_recycle_delete: bool,
        is_recycle_bin_op: bool,
        is_empty_recycle: bool,
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
    history: NavHistory,
    all_entries: Vec<FileEntry>,
    entries: Vec<FileEntry>,
    selected_index: Option<usize>,
    pending_selection: Option<PathBuf>,
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
            history: NavHistory::with_initial(path.clone()),
            path,
            all_entries: Vec::new(),
            entries: Vec::new(),
            selected_index: None,
            pending_selection: None,
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

/// Timeout after which incremental type-ahead in the file list starts a fresh prefix.
const TYPE_AHEAD_TIMEOUT: Duration = Duration::from_millis(850);

/// Tracks incremental keyboard type-ahead prefix selection in the active file pane.
#[derive(Debug, Default)]
struct TypeAheadState {
    buffer: String,
    last_typed: Option<std::time::Instant>,
    pane: Option<PaneId>,
}

/// Extracts a printable type-ahead character from a file-list keystroke, ignoring
/// modifier shortcuts (`Ctrl`/`Alt`/`Win`) and non-name control keys.
fn type_ahead_char(ev: &KeyDownEvent) -> Option<char> {
    let mods = &ev.keystroke.modifiers;
    if mods.control || mods.alt || mods.platform || mods.function {
        return None;
    }
    if let Some(key_char) = ev.keystroke.key_char.as_deref() {
        let mut chars = key_char.chars();
        if let (Some(ch), None) = (chars.next(), chars.next())
            && (ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.'))
        {
            return Some(ch);
        }
    }
    let mut chars = ev.keystroke.key.chars();
    if let (Some(ch), None) = (chars.next(), chars.next())
        && (ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        return Some(ch);
    }
    None
}

/// Finds the next entry in `entries` whose name starts with `query` (case-insensitive).
///
/// - When `cycle_same_char` is `true` (e.g. pressing `d` repeatedly), advances past
///   `current` and wraps around so the user cycles through all matching items.
/// - For a multi-character prefix (e.g. `doc`), keeps `current` if it already matches,
///   otherwise searches forward from `current` (or index `0`) with wrap-around.
/// - For a fresh single-character press, jumps to the first match starting from index `0`
///   unless `current` already starts with that character, in which case it advances to
///   the next match.
fn find_type_ahead_match(
    entries: &[FileEntry],
    query: &str,
    current: Option<usize>,
    cycle_same_char: bool,
) -> Option<usize> {
    if entries.is_empty() || query.is_empty() {
        return None;
    }
    let needle = query.to_lowercase();
    let len = entries.len();
    let matches_at = |idx: usize| entries[idx].name.to_lowercase().starts_with(&needle);

    let start = if cycle_same_char {
        current.map_or(0, |idx| (idx + 1) % len)
    } else if needle.chars().count() == 1 {
        if let Some(idx) = current
            && idx < len
            && matches_at(idx)
        {
            (idx + 1) % len
        } else {
            0
        }
    } else {
        current.filter(|&idx| idx < len).unwrap_or(0)
    };

    (0..len)
        .map(|offset| (start + offset) % len)
        .find(|&idx| matches_at(idx))
}

/// Main GPUI view for Onyx Stage 1 (visual parity with Slint UI).
pub struct OnyxExplorer {
    pub(crate) runtime: Arc<Runtime>,
    pub(crate) theme: OnyxTheme,
    pub(crate) focus_handle: FocusHandle,
    /// Focus handles of the two file panes; the active pane owns the keyboard so
    /// the screen reader reads its list and rows while navigating.
    pub(crate) pane_focus: [FocusHandle; 2],
    /// Focus rings of the modal surfaces, used for `Tab` trapping and focus return.
    pub(crate) modals: Modals,
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
    pub(crate) group_by: GroupByMode,
    pub(crate) collapsed_groups: HashSet<String>,
    pub(crate) show_hidden: bool,
    pub(crate) calculate_folder_sizes: bool,
    pub(crate) sort_spec: SortSpec,
    pub(crate) all_entries: Vec<FileEntry>,
    pub(crate) entries: Vec<FileEntry>,
    pub(crate) selected_index: Option<usize>,
    type_ahead: TypeAheadState,
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
    /// Items of the Recycle Bin listing, kept beside the rendered rows because a
    /// recycled item has no live path of its own to restore to.
    pub(crate) recycle_items: Vec<RecycleBinItem>,
    /// Whether the Windows Recycle Bin was last observed to hold at least one item,
    /// used to select the sidebar's `:empty` vs `:full` stock icon even outside bin view.
    pub(crate) recycle_bin_has_items: bool,
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
    rebuild_pending: Option<Vec<PathBuf>>,
    /// Hit selected by the command palette, revealed once its folder loaded.
    pending_selection: Option<PathBuf>,
    pub(crate) list_scroll_handle: ScrollHandle,
    pub(crate) list_scroll: SmoothScroll,
    /// Set while a frame callback is armed for the scroll glide; render re-arms
    /// it each frame so the easing runs on the display clock.
    pub(crate) scroll_frame_armed: bool,
    /// Files dragged in from another application, tracked from `Entered` until
    /// the platform reports `Submit`.
    external_drop: Option<ExternalDropIn>,
    pub(crate) grid_scroll_handle: UniformListScrollHandle,
    pub(crate) operations: Arc<FileOperationsEngine>,
    clipboard_tx: mpsc::UnboundedSender<ClipboardCommand>,
    shutdown_token: CancellationToken,
    pub(crate) can_paste: bool,
    pub(crate) can_undo: bool,
    pub(crate) status_message: Option<String>,
    pub(crate) prompt_state: Option<PromptState>,
    pub(crate) conflict_dialog: Option<ConflictDialogState>,
    pending_conflict_check: bool,
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
    pub(crate) preview_side: PreviewSide,
    pub(crate) pane_split_ratio: f32,
    pub(crate) splitter_drag: Option<ActiveSplitterDrag>,
    pub(crate) command_palette: Option<CommandPaletteState>,
    pub(crate) settings_open: bool,
    pub(crate) archive: Option<ArchiveViewState>,
    archive_generation: GenerationId,
    archive_cancel: CancellationToken,
    pub(crate) drag: Option<DragState>,
    /// Set while the OLE drag loop runs, so pointer crossings of the window edge
    /// cannot start a second drag and self-drops back into Onyx are safely ignored.
    external_drag_active: bool,
    /// Set when the user drags an item out of Onyx, drags it back over Onyx, and
    /// releases it inside our own window so `finish_external_drag` treats it as cancelled.
    external_drag_returned_inside: bool,
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

        // The executable carries the Onyx icon as resource 1 (see `build.rs`); pushing it
        // onto the window gives the taskbar and Alt-Tab the same mark. Off the UI thread
        // because loading the icon hits the image loader.
        if let Some(hwnd) = extract_window_hwnd(window) {
            let runtime = Arc::clone(&runtime);
            runtime.spawn_blocking(move || {
                let _ = onyx_windows::set_window_icon_from_exe(hwnd);
            });
        }

        let focus_handle = cx.focus_handle();
        let prompt_focus_handle = cx.focus_handle();
        let pane_focus = [cx.focus_handle(), cx.focus_handle()];
        let modals = Modals::new(cx);
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
            pane_focus,
            modals,
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
            group_by: GroupByMode::None,
            collapsed_groups: HashSet::new(),
            show_hidden: false,
            calculate_folder_sizes: true,
            sort_spec: SortSpec::default(),
            all_entries: Vec::new(),
            entries: Vec::new(),
            selected_index: None,
            type_ahead: TypeAheadState::default(),
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
            recycle_items: Vec::new(),
            recycle_bin_has_items: false,
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
            external_drop: None,
            grid_scroll_handle: UniformListScrollHandle::new(),
            operations: Arc::new(FileOperationsEngine::new()),
            clipboard_tx,
            shutdown_token,
            can_paste: false,
            can_undo: false,
            status_message: None,
            prompt_state: None,
            conflict_dialog: None,
            pending_conflict_check: false,
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
            preview_side: PreviewSide::Right,
            pane_split_ratio: onyx_config::DEFAULT_PANE_SPLIT_RATIO,
            splitter_drag: None,
            command_palette: None,
            settings_open: false,
            archive: None,
            archive_generation: GenerationId::new(0),
            archive_cancel: CancellationToken::new(),
            drag: None,
            external_drag_active: false,
            external_drag_returned_inside: false,
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
        window.focus(&explorer.pane_focus[0], cx);
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
        match self.active_pane {
            PaneId::Primary => self.tabs.active_tab().map(|tab| &tab.history),
            PaneId::Secondary => Some(&self.secondary.history),
        }
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
        } else if self.search_run.is_busy()
            || self
                .command_palette
                .as_ref()
                .is_some_and(|palette| palette.search.run.is_busy())
        {
            let matches = match self.search_run.status() {
                SearchStatus::Running { matches } => *matches,
                _ => self
                    .command_palette
                    .as_ref()
                    .and_then(|palette| match palette.search.run.status() {
                        SearchStatus::Running { matches } => Some(*matches),
                        _ => None,
                    })
                    .unwrap_or(0),
            };
            if matches > 0 {
                format!("Searching ({matches})")
            } else {
                "Searching".to_string()
            }
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

    pub fn set_active_pane(&mut self, pane: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_pane == pane {
            return;
        }
        self.active_pane = pane;
        // Keyboard focus follows the active pane so assistive technology keeps
        // reading the list the user is navigating.
        window.focus(self.pane_focus_handle(pane), cx);
        self.update_preview(cx);
        cx.notify();
    }

    /// Focus handle of a file pane, used by its list container.
    pub(crate) fn pane_focus_handle(&self, pane: PaneId) -> &FocusHandle {
        match pane {
            PaneId::Primary => &self.pane_focus[0],
            PaneId::Secondary => &self.pane_focus[1],
        }
    }

    pub fn toggle_dual_pane(&mut self, cx: &mut Context<Self>) {
        self.dual_pane = !self.dual_pane;
        self.layout_touched = true;
        if self.dual_pane {
            let path = self.secondary.path.clone();
            self.load_secondary_directory(path, false, true, cx);
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

    pub fn toggle_preview_side(&mut self, cx: &mut Context<Self>) {
        self.preview_side = self.preview_side.toggled();
        self.layout_touched = true;
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
        self.load_secondary_directory(primary, false, true, cx);
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
                    self.preview_side == PreviewSide::Left,
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

                // Recycle Bin is a Shell namespace folder: its icon comes from the
                // stock icon set, one key per state (`:empty` / `:full`).
                explorer.refresh_recycle_bin_icons(cx);

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

    /// Moves a tab between Primary and Secondary panes in Dual-Pane mode,
    /// preserving path, `NavHistory`, `SortSpec`, and selection.
    ///
    /// - `tab_id = Some(id)` moves that specific primary tab to the secondary pane.
    /// - `tab_id = None` moves whichever pane is currently `self.active_pane`.
    /// - No-ops gracefully when Dual-Pane mode is off.
    pub fn move_tab_to_other_pane(&mut self, tab_id: Option<u64>, cx: &mut Context<Self>) {
        if !self.dual_pane {
            return;
        }
        let primary_tab_index = match tab_id {
            Some(id) => {
                let Some(idx) = self.tabs.tabs().iter().position(|t| t.id == id) else {
                    return;
                };
                Some(idx)
            }
            None => None,
        };

        let primary_selected = self
            .selected_index
            .and_then(|idx| self.entries.get(idx))
            .map(|entry| entry.path.clone())
            .or_else(|| self.pending_selection.clone());
        if let Some(tab) = self.tabs.active_tab_mut() {
            tab.sort = self.sort_spec;
            tab.selected_paths = primary_selected.into_iter().collect();
        }

        let secondary_selected = self
            .secondary
            .selected_index
            .and_then(|idx| self.secondary.entries.get(idx))
            .map(|entry| entry.path.clone())
            .or_else(|| self.secondary.pending_selection.clone());
        let mut secondary_tab = TabState::new(0, self.secondary.path.clone());
        secondary_tab.history = self.secondary.history.clone();
        secondary_tab.sort = self.secondary.sort_spec;
        secondary_tab.selected_paths = secondary_selected.into_iter().collect();

        if !transfer_tab_between_panes(
            &mut self.tabs,
            &mut secondary_tab,
            &mut self.active_pane,
            primary_tab_index,
        ) {
            return;
        }

        let secondary_path = secondary_tab.path.clone();
        let secondary_sel = secondary_tab.selected_paths.first().cloned();
        self.secondary.path = secondary_path.clone();
        self.secondary.history = secondary_tab.history;
        self.secondary.sort_spec = secondary_tab.sort;
        self.secondary.pending_selection = secondary_sel;

        let primary_tab = self.tabs.active_tab().cloned();
        let (primary_path, primary_sel) = if let Some(tab) = primary_tab {
            self.sort_spec = tab.sort;
            (tab.path, tab.selected_paths.first().cloned())
        } else {
            (self.home_path.clone(), None)
        };
        self.pending_selection = primary_sel.clone();
        self.address_editing = false;
        self.load_directory(primary_path, false, true, cx);
        if let Some(sel) = primary_sel
            && let Some(tab) = self.tabs.active_tab_mut()
        {
            tab.selected_paths = vec![sel];
        }
        self.load_secondary_directory(secondary_path, false, true, cx);
        self.persist_session();
        cx.notify();
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
        match self.active_pane {
            PaneId::Primary => {
                let target = self
                    .tabs
                    .active_tab_mut()
                    .and_then(|tab| tab.history.go_back().cloned());
                if let Some(target) = target {
                    self.load_directory(target, false, true, cx);
                }
            }
            PaneId::Secondary => {
                if let Some(target) = self.secondary.history.go_back().cloned() {
                    self.load_secondary_directory(target, false, true, cx);
                }
            }
        }
    }

    pub fn go_forward(&mut self, cx: &mut Context<Self>) {
        match self.active_pane {
            PaneId::Primary => {
                let target = self
                    .tabs
                    .active_tab_mut()
                    .and_then(|tab| tab.history.go_forward().cloned());
                if let Some(target) = target {
                    self.load_directory(target, false, true, cx);
                }
            }
            PaneId::Secondary => {
                if let Some(target) = self.secondary.history.go_forward().cloned() {
                    self.load_secondary_directory(target, false, true, cx);
                }
            }
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
            PaneId::Secondary => self.load_secondary_directory(parent, true, true, cx),
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
                self.load_secondary_directory(path, false, false, cx);
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
        // The Recycle Bin is a Shell namespace folder without a filesystem path, so
        // its tab path is routed to the Shell enumeration instead of `onyx-fs`.
        if Self::is_recycle_bin_path(&path) {
            self.load_recycle_bin(record_history, cx);
            return;
        }
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
        // A folder listing always leaves the Recycle Bin list behind.
        if self.list_mode == ListMode::RecycleBin {
            self.list_mode = ListMode::Browse;
            self.recycle_items.clear();
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

    // ---- Windows Recycle Bin (Shell namespace folder) ----

    /// Shell namespace path of the Recycle Bin.
    ///
    /// The bin has no filesystem path of its own, so the tab carries this identifier
    /// and [`Self::pane_location_label`] shows the plain name for it.
    #[must_use]
    pub fn recycle_bin_path() -> PathBuf {
        PathBuf::from(r"::{645FF040-5081-101B-9F08-00AA002F954E}")
    }

    /// Whether `path` is the Recycle Bin's Shell namespace path.
    #[must_use]
    fn is_recycle_bin_path(path: &Path) -> bool {
        same_path(path, &Self::recycle_bin_path())
    }

    /// Whether the primary pane — the only pane that can show it — lists the Recycle Bin.
    #[must_use]
    pub fn is_recycle_bin_mode(&self) -> bool {
        self.list_mode == ListMode::RecycleBin
    }

    /// Whether `pane` currently lists the Recycle Bin.
    #[must_use]
    pub fn pane_shows_recycle_bin(&self, pane: PaneId) -> bool {
        self.is_recycle_bin_mode() && pane == PaneId::Primary
    }

    /// Location label of one pane, used by the breadcrumbs, the pane header and the
    /// status bar; the Recycle Bin shows its name instead of the namespace path.
    #[must_use]
    pub fn pane_location_label(&self, pane: PaneId) -> String {
        if self.pane_shows_recycle_bin(pane) {
            "Recycle Bin".to_string()
        } else {
            normalize_display_path(self.pane_path(pane))
        }
    }

    /// Selected Recycle Bin item, when one is selected in Recycle Bin mode.
    #[must_use]
    pub fn selected_recycle_item(&self) -> Option<&RecycleBinItem> {
        if !self.is_recycle_bin_mode() {
            return None;
        }
        let entry = self.selected_entry()?;
        self.recycle_items
            .iter()
            .find(|item| item.shell_path == entry.path)
    }

    /// Original location of the selected Recycle Bin item, when one is selected.
    #[must_use]
    pub fn selected_recycle_original(&self) -> Option<&Path> {
        self.selected_recycle_item()?.original_path.as_deref()
    }

    /// Whether the currently selected Recycle Bin item can be restored.
    #[must_use]
    pub fn can_restore_selection(&self) -> bool {
        self.active_operation.is_none()
            && self
                .selected_recycle_item()
                .is_some_and(can_restore_recycle_item)
    }

    /// Whether the Recycle Bin currently has items that can be emptied.
    #[must_use]
    pub fn can_empty_recycle_bin(&self) -> bool {
        self.is_recycle_bin_mode()
            && !self.recycle_items.is_empty()
            && self.active_operation.is_none()
    }

    /// Re-extracts the empty/full Windows Recycle Bin stock icons on a blocking worker
    /// so the sidebar reflects the current bin state without blocking the UI thread.
    fn refresh_recycle_bin_icons(&mut self, cx: &mut Context<Self>) {
        let recycle_work: Vec<(String, PathBuf, bool)> = [":empty", ":full"]
            .iter()
            .map(|state| {
                let key = format!("{RECYCLE_BIN_ICON_PREFIX}{state}");
                self.icons.mark_pending(key.clone());
                (key, PathBuf::new(), true)
            })
            .collect();
        self.spawn_icon_batch(recycle_work, cx);
    }

    /// Restores the currently selected Recycle Bin item back to its original location.
    pub fn restore_selected_recycle_item(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.selected_recycle_item().cloned() else {
            return;
        };
        self.restore_recycle_item(item, cx);
    }

    /// Restores the Recycle Bin item at visible row `idx` back to its original location.
    pub fn restore_recycle_item_at(&mut self, idx: usize, cx: &mut Context<Self>) {
        if !self.is_recycle_bin_mode() {
            return;
        }
        let Some(entry) = self.entries.get(idx) else {
            return;
        };
        let Some(item) = self
            .recycle_items
            .iter()
            .find(|item| item.shell_path == entry.path)
            .cloned()
        else {
            return;
        };
        self.restore_recycle_item(item, cx);
    }

    fn restore_recycle_item(&mut self, item: RecycleBinItem, cx: &mut Context<Self>) {
        if !can_restore_recycle_item(&item) {
            self.status_message = Some(format!(
                "Cannot restore \"{}\": original location is unknown.",
                item.name
            ));
            cx.notify();
            return;
        }
        self.start_operation(FileOperation::RestoreRecycleItems(vec![item]), cx);
    }

    /// Opens the explicit danger confirmation modal before emptying the Recycle Bin.
    pub fn prompt_empty_recycle_bin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_recycle_bin_mode() {
            return;
        }
        if self.recycle_items.is_empty() {
            self.status_message = Some("Recycle Bin is already empty.".to_string());
            cx.notify();
            return;
        }
        let message = empty_recycle_bin_prompt_message(self.recycle_items.len());
        self.open_prompt(
            PromptKind::EmptyRecycleBin,
            "Empty Recycle Bin?",
            &message,
            "",
            "Empty Recycle Bin",
            window,
            cx,
        );
    }

    /// Opens the Windows Recycle Bin in the primary pane.
    pub fn open_recycle_bin(&mut self, cx: &mut Context<Self>) {
        if self.is_recycle_bin_mode() {
            self.refresh(cx);
            return;
        }
        self.navigate_to(Self::recycle_bin_path(), cx);
    }

    /// Fills the primary pane with the Recycle Bin listing.
    ///
    /// The Shell enumeration runs on the blocking pool; its result is dropped when the
    /// pane moved on, so a fast navigation never shows a stale bin.
    fn load_recycle_bin(&mut self, record_history: bool, cx: &mut Context<Self>) {
        if !self.is_recycle_bin_mode() {
            self.context_menu = None;
            self.reset_list_scroll(PaneId::Primary);
        }
        self.address_editing = false;
        self.address_suggestions.clear();
        self.suggestion_index = None;

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
            tab.navigate(Self::recycle_bin_path(), record_history);
            self.sort_spec = tab.sort;
        }
        // The bin is a list source of its own: neither a search result nor the browse
        // list of a folder on disk.
        self.cancel_list_search();
        self.list_mode = ListMode::RecycleBin;
        self.search_hits.clear();
        self.recycle_items.clear();
        self.all_entries.clear();
        self.entries.clear();
        self.selected_index = None;
        self.error_message = None;
        self.is_loading = true;
        // Shell namespace items are not watched; the previous folder keeps its watcher
        // and its events no longer match the open location.
        self.watched_path = None;
        cx.notify();

        let (tx, rx) = oneshot::channel();
        self.runtime.spawn_blocking(move || {
            let _ = tx.send(enumerate_recycle_bin());
        });

        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |explorer, cx| {
                explorer.finish_recycle_load(generation, result, cx);
            });
        })
        .detach();
    }

    /// Applies the enumeration of `generation`; superseded results are dropped.
    fn finish_recycle_load(
        &mut self,
        generation: GenerationId,
        result: OnyxResult<Vec<RecycleBinItem>>,
        cx: &mut Context<Self>,
    ) {
        if self.generation != generation || !self.is_recycle_bin_mode() {
            return;
        }
        self.is_loading = false;
        match result {
            Ok(items) => {
                self.recycle_items = items;
                self.recycle_bin_has_items = !self.recycle_items.is_empty();
                self.all_entries = self.recycle_items.iter().map(recycle_bin_row).collect();
                self.error_message = None;
                self.apply_filter_and_sort();
                self.selected_index = if self.entries.is_empty() {
                    None
                } else {
                    Some(0)
                };
                self.refresh_recycle_bin_icons(cx);
                self.queue_entry_icons(cx);
                self.update_preview(cx);
            }
            Err(err) => {
                self.recycle_items.clear();
                self.all_entries.clear();
                self.entries.clear();
                self.selected_index = None;
                self.error_message = Some(err.to_string());
            }
        }
        cx.notify();
    }

    /// Refuses a filesystem action while the Recycle Bin is on screen.
    ///
    /// Recycled items have no live path to act on, so the refusal explains itself in
    /// the status bar instead of failing deep inside an operation.
    fn refuse_in_recycle_bin(&mut self, action: &str, cx: &mut Context<Self>) -> bool {
        if !self.is_recycle_bin_mode() {
            return false;
        }
        self.status_message = Some(format!("{action} is not available in the Recycle Bin yet."));
        cx.notify();
        true
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
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    /// Moves the keyboard focus to the list of the active file pane, the surface
    /// that owns arrow-key navigation.
    pub fn focus_active_pane(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(self.pane_focus_handle(self.active_pane), cx);
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
        self.focus_active_pane(window, cx);

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
        // The Recycle Bin list is filtered in memory: an index run walks filesystem
        // roots and cannot reach Shell namespace items.
        if self.indexed_search
            && !self.is_recycle_bin_mode()
            && !self.search_query.trim().is_empty()
        {
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
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    /// Leaves the search list and restores the browse list of the open folders.
    fn cancel_list_search(&mut self) {
        self.search_run.cancel();
        self.search_hits.clear();
        if self.list_mode == ListMode::Search {
            self.list_mode = ListMode::Browse;
        }
    }

    /// Starts one streaming search run over the open folder tree and feeds
    /// matched batches into the primary pane as they arrive. The previous run
    /// is cancelled immediately, so a new keystroke or navigation aborts the
    /// old stream and drops any stale batches.
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
        let (tx, mut rx) = mpsc::channel(search_index::STREAM_EVENT_CHANNEL_CAPACITY);
        let worker_cancel = cancel.clone();
        self.runtime.spawn(async move {
            search_index::run_query_streaming(
                engine,
                generation,
                root,
                query,
                worker_cancel,
                debounce,
                tx,
            )
            .await;
        });
        cx.spawn(async move |this, cx| {
            while let Some(event) = rx.recv().await {
                let keep_going = this
                    .update(cx, |explorer, cx| {
                        explorer.handle_list_search_event(event, cx)
                    })
                    .unwrap_or(false);
                if !keep_going {
                    cancel.cancel();
                    break;
                }
            }
        })
        .detach();

        self.ensure_led_animation(cx);
        cx.notify();
    }

    /// Applies one incremental streaming search event for the primary pane.
    /// Events of a superseded generation are dropped; returns `true` while the
    /// stream for this generation remains active.
    fn handle_list_search_event(
        &mut self,
        event: SearchStreamEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        match event {
            SearchStreamEvent::Batch {
                generation,
                results,
            } => {
                let Some(hits) = self.search_run.accept_batch(generation, results) else {
                    return false;
                };
                // The index ignores the listing options of the browse view, so protect
                // the protected-system-file preference here, as the Slint app does.
                let show_system = self.config.show_system;
                let filtered: Vec<FileEntry> = hits
                    .into_iter()
                    .filter(|entry| show_system || !entry.attributes.system)
                    .collect();
                if !filtered.is_empty() {
                    let was_empty = self.entries.is_empty();
                    self.search_hits.extend(filtered.iter().cloned());
                    self.entries.extend(filtered);
                    if was_empty && self.selected_index.is_none() {
                        self.selected_index = Some(0);
                        self.update_preview(cx);
                    }
                    self.queue_entry_icons(cx);
                }
                cx.notify();
                true
            }
            SearchStreamEvent::Finished {
                generation,
                outcome,
            } => {
                if !self.search_run.finish_stream(generation, outcome) {
                    return false;
                }
                if let SearchStatus::Failed { message } = self.search_run.status() {
                    self.error_message = Some(message.clone());
                } else if self.selected_index.is_none() && !self.entries.is_empty() {
                    self.selected_index = Some(0);
                }
                self.update_preview(cx);
                self.queue_entry_icons(cx);
                self.queue_thumbnails(cx);
                cx.notify();
                false
            }
        }
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
            // Index the configured roots, not just the folder that happens to
            // be open: a search from elsewhere has to find these files too.
            self.request_index_build(self.index_roots(), cx);
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
        let roots = self.index_roots();
        let names = roots
            .iter()
            .filter_map(|root| {
                root.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .collect::<Vec<_>>()
            .join(", ");
        self.status_message = Some(format!("Rebuilding index for {names}…"));
        self.request_index_build(roots, cx);
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
            SearchStatus::Running { matches: 0 } => "Searching…".to_string(),
            SearchStatus::Running { matches } => format!("Searching… ({matches})"),
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
                if let Some(roots) = self.rebuild_pending.take() {
                    self.start_index_build(roots, cx);
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
    fn request_index_build(&mut self, roots: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.search_index.is_some() {
            self.start_index_build(roots, cx);
        } else {
            // The worker still opens the database; build as soon as it is ready.
            self.rebuild_pending = Some(roots);
            cx.notify();
        }
    }

    /// Runs `index_directory` plus a reconcile of `root` on the blocking pool,
    /// never on the UI thread.
    fn start_index_build(&mut self, roots: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(shared) = self.search_index.clone() else {
            self.rebuild_pending = Some(roots);
            return;
        };
        let Some(root) = roots.first().cloned() else {
            return;
        };
        self.rebuild_pending = None;
        self.build_cancel.cancel();
        let cancel = CancellationToken::new();
        self.build_cancel = cancel.clone();
        self.build_generation = self.build_generation.next();
        let generation = self.build_generation;
        self.index_status = IndexStatus::Building { root: root.clone() };

        let build_roots = roots.clone();
        let (tx, rx) = oneshot::channel();
        self.runtime.spawn(async move {
            let indexed = tokio::task::spawn_blocking(move || -> OnyxResult<usize> {
                if cancel.is_cancelled() {
                    return Err(OnyxError::Cancelled);
                }
                let guard = shared.lock().map_err(|_| OnyxError::Search {
                    message: "search index mutex was poisoned".to_string(),
                })?;
                let mut total = 0usize;
                for root in &build_roots {
                    if cancel.is_cancelled() {
                        return Err(OnyxError::Cancelled);
                    }
                    total += guard.index_directory(root, &cancel)?;
                    guard.reconcile_directory_with_cancel(root, &cancel)?;
                }
                Ok(total)
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
        let needle = self.search_query.trim();
        let mut primary = build_view_entries(
            &self.all_entries,
            self.sort_spec,
            needle,
            &self.folder_sizes,
        );
        let mut secondary = build_view_entries(
            &self.secondary.all_entries,
            self.secondary.sort_spec,
            needle,
            &self.folder_sizes,
        );
        if self.group_by != GroupByMode::None {
            primary = group_entries(&primary, self.group_by)
                .into_iter()
                .flat_map(|g| g.entries)
                .collect();
            secondary = group_entries(&secondary, self.group_by)
                .into_iter()
                .flat_map(|g| g.entries)
                .collect();
        }
        self.entries = primary;
        self.secondary.entries = secondary;
    }

    /// Short display label for a [`GroupByMode`].
    #[must_use]
    pub const fn group_by_label(mode: GroupByMode) -> &'static str {
        match mode {
            GroupByMode::None => "Group",
            GroupByMode::Kind => "Group: Type",
            GroupByMode::Extension => "Group: Ext",
            GroupByMode::ModifiedDate => "Group: Date",
            GroupByMode::Size => "Group: Size",
        }
    }

    /// Cycles the active Group-By mode (`None -> Kind -> Extension -> ModifiedDate -> Size -> None`).
    pub fn cycle_group_by(&mut self, cx: &mut Context<Self>) {
        self.group_by = match self.group_by {
            GroupByMode::None => GroupByMode::Kind,
            GroupByMode::Kind => GroupByMode::Extension,
            GroupByMode::Extension => GroupByMode::ModifiedDate,
            GroupByMode::ModifiedDate => GroupByMode::Size,
            GroupByMode::Size => GroupByMode::None,
        };
        self.collapsed_groups.clear();
        self.apply_filter_and_sort();
        self.status_message = Some(match self.group_by {
            GroupByMode::None => "Grouping turned off.".to_string(),
            mode => format!("Grouped by {}.", Self::group_by_label(mode)),
        });
        cx.notify();
    }

    /// Toggles whether a group section header is collapsed in Details view.
    pub fn toggle_group_collapsed(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.collapsed_groups.remove(key) {
            self.collapsed_groups.insert(key.to_string());
        }
        cx.notify();
    }

    /// Returns whether the given group key is currently collapsed.
    #[must_use]
    pub fn is_group_collapsed(&self, key: &str) -> bool {
        self.collapsed_groups.contains(key)
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

    pub fn toggle_quick_look(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.quick_look_open {
            self.close_quick_look(window, cx);
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
        a11y::open_modal(self, ModalSurface::QuickLook, 0, window, cx);
        self.update_preview(cx);
        cx.notify();
    }

    pub fn close_quick_look(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.quick_look_open {
            self.quick_look_open = false;
            a11y::close_modal(self, ModalSurface::QuickLook, window, cx);
            self.update_preview(cx);
            cx.notify();
        }
    }

    pub fn open_selected_from_quick_look(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.active_pane;
        let idx = self.pane_selected_index(pane);
        self.close_quick_look(window, cx);
        if let Some(idx) = idx {
            self.activate_entry_in(pane, idx, window, cx);
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
            self.preview_side = session.preview_side;
            self.pane_split_ratio = session.pane_split_ratio;
            self.collapsed_sidebar_groups =
                session.collapsed_sidebar_groups.iter().cloned().collect();
            // The stored session wins; `dual_pane_default` only opens a second
            // pane when the session was saved without one.
            self.dual_pane = session.dual_pane || self.config.dual_pane_default;
        }
        if let Some(path) = session.secondary_path.clone() {
            self.secondary.path = path.clone();
            self.secondary.history = NavHistory::with_initial(path);
        }
        if self.dual_pane {
            let path = self.secondary.path.clone();
            self.load_secondary_directory(path, false, true, cx);
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
        let preview_side = self.preview_side;
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
            session.preview_side = preview_side;
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

    fn load_secondary_directory(
        &mut self,
        path: PathBuf,
        record_history: bool,
        use_cache: bool,
        cx: &mut Context<Self>,
    ) {
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
        if record_history {
            self.secondary.history.push(path.clone());
        }
        self.secondary.selected_index = None;
        self.secondary.error_message = None;
        self.secondary.is_loading = true;
        self.sync_secondary_watcher();
        self.update_preview(cx);

        if use_cache && let Some(mut cached) = self.directories.get(&path) {
            sort_entries(&mut cached, self.secondary.sort_spec);
            self.secondary.all_entries = cached;
            self.apply_filter_and_sort();
            self.apply_secondary_pending_selection(cx);
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
                        explorer.apply_secondary_pending_selection(cx);
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

    pub fn activate_entry(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_entry_in(self.active_pane, idx, window, cx);
    }

    pub fn activate_entry_in(
        &mut self,
        pane: PaneId,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.pane_entries(pane).get(idx).cloned() else {
            return;
        };
        // Activating a recycled item (Enter / double-click) restores it to its original location.
        if self.pane_shows_recycle_bin(pane) {
            self.restore_recycle_item_at(idx, cx);
            return;
        }
        if entry.is_dir() {
            match pane {
                PaneId::Primary => self.load_directory(entry.path, true, true, cx),
                PaneId::Secondary => self.load_secondary_directory(entry.path, true, true, cx),
            }
        } else if entry_is_archive(&entry) {
            self.open_archive_browser(entry.path, window, cx);
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
        if let Some(tab) = self.tabs.active_tab_mut() {
            tab.selected_paths = vec![path];
        }
        if self.view_mode != ViewMode::Grid {
            self.reveal_list_row(PaneId::Primary, index, cx);
        }
        self.update_preview(cx);
    }

    /// Selects and reveals the carried-over secondary selection once entries load.
    fn apply_secondary_pending_selection(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.secondary.pending_selection.clone() else {
            return;
        };
        let Some(index) = self
            .secondary
            .entries
            .iter()
            .position(|entry| same_path(&entry.path, &path))
        else {
            return;
        };
        self.secondary.pending_selection = None;
        self.secondary.selected_index = Some(index);
        if self.view_mode != ViewMode::Grid {
            self.reveal_list_row(PaneId::Secondary, index, cx);
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

    /// Handles incremental letter/digit type-ahead jump in the active file pane.
    fn handle_type_ahead(&mut self, ch: char, cx: &mut Context<Self>) -> bool {
        let pane = self.active_pane;
        if self.pane_entries(pane).is_empty() {
            return false;
        }
        let now = std::time::Instant::now();
        let expired = self.type_ahead.pane != Some(pane)
            || self
                .type_ahead
                .last_typed
                .is_none_or(|ts| now.duration_since(ts) > TYPE_AHEAD_TIMEOUT);
        if expired {
            self.type_ahead.buffer.clear();
        }
        let ch_lower: String = ch.to_lowercase().collect();
        let cycle_same_char = !self.type_ahead.buffer.is_empty()
            && self.type_ahead.buffer.chars().count() == 1
            && self.type_ahead.buffer == ch_lower;
        if !cycle_same_char {
            self.type_ahead.buffer.push_str(&ch_lower);
        }
        self.type_ahead.last_typed = Some(now);
        self.type_ahead.pane = Some(pane);

        let current = self.pane_selected_index(pane);
        let query = self.type_ahead.buffer.clone();
        let mut matched =
            find_type_ahead_match(self.pane_entries(pane), &query, current, cycle_same_char);
        if matched.is_none() && query.chars().count() > 1 {
            // Fall back to starting a fresh single-character jump if the longer prefix missed.
            self.type_ahead.buffer = ch_lower;
            matched = find_type_ahead_match(
                self.pane_entries(pane),
                &self.type_ahead.buffer,
                current,
                false,
            );
        }

        let Some(idx) = matched else {
            return false;
        };
        match pane {
            PaneId::Primary => self.selected_index = Some(idx),
            PaneId::Secondary => self.secondary.selected_index = Some(idx),
        }
        if self.view_mode != ViewMode::Grid {
            self.reveal_list_row(pane, idx, cx);
        }
        self.update_preview(cx);
        cx.notify();
        true
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
                let icon = if key.starts_with(RECYCLE_BIN_ICON_PREFIX) {
                    // Windows stock icons: they have no path, the key carries the state.
                    recycle_bin_icon_rgba(key.ends_with(":full"))
                } else if key.starts_with("path:") {
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
        // The Shell reports the size of a recycled folder itself, and its items have
        // no live path to walk.
        if self.is_recycle_bin_mode() {
            return;
        }
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
        let is_danger = kind.is_danger();
        let show_input = !is_danger;
        self.prompt_state = Some(PromptState {
            kind,
            title: title.to_string(),
            message: message.to_string(),
            confirm_label: confirm_label.to_string(),
            is_danger,
            show_input,
        });
        a11y::open_modal(self, ModalSurface::Prompt, 0, window, cx);
        if show_input {
            let value = initial_input.to_string();
            self.prompt_input.update(cx, |input, cx| {
                input.set_value(value, window, cx);
                input.focus(window, cx);
            });
        } else {
            window.focus(&self.prompt_focus_handle, cx);
            a11y::focus_modal_slot(self, ModalSurface::Prompt, 0, window, cx);
        }
        cx.notify();
    }

    /// Walks the prompt dialog stops: the name field (when it has one), Cancel
    /// and Confirm. The text field lives in an `InputState`, so it is focused by
    /// entity rather than through the ring slot it occupies.
    fn step_prompt_focus(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let show_input = self.prompt_state.as_ref().is_some_and(|p| p.show_input);
        let input_focused = show_input
            && self
                .prompt_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window);
        let focused_slot = a11y::modal_focused_slot(self, ModalSurface::Prompt, window);
        let current = if input_focused {
            Some(0)
        } else {
            focused_slot.map(|slot| if show_input { slot + 1 } else { slot })
        };
        let count = if show_input { 3 } else { 2 };
        let next = a11y::next_stop(current, forward, count);
        if show_input && next == 0 {
            self.prompt_input
                .update(cx, |input, cx| input.focus(window, cx));
        } else {
            let slot = if show_input { next - 1 } else { next };
            a11y::focus_modal_slot(self, ModalSurface::Prompt, slot, window, cx);
        }
    }

    pub fn cancel_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_state = None;
        a11y::close_modal(self, ModalSurface::Prompt, window, cx);
        cx.notify();
    }

    pub fn confirm_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = self.prompt_state.take() else {
            return;
        };
        let input_text = self.prompt_input.read(cx).value().to_string();
        a11y::close_modal(self, ModalSurface::Prompt, window, cx);

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
            PromptKind::EmptyRecycleBin => FileOperation::EmptyRecycleBin,
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
        self.initiate_paste(destination, cx);
    }

    fn initiate_paste(&mut self, destination: PathBuf, cx: &mut Context<Self>) {
        if self.refuse_in_recycle_bin("Editing recycled items", cx) {
            return;
        }
        if self.active_operation.is_some()
            || self.conflict_dialog.is_some()
            || self.pending_conflict_check
        {
            self.status_message = Some("A file operation is already in progress.".to_string());
            cx.notify();
            return;
        }
        self.pending_conflict_check = true;
        let clipboard = self.clipboard_tx.clone();
        let runtime = Arc::clone(&self.runtime);
        let (tx, rx) = oneshot::channel();

        runtime.spawn(async move {
            let (reply, answer) = oneshot::channel();
            let _ = clipboard.send(ClipboardCommand::Read(reply));
            let files = answer.await.ok().flatten();
            let result = match files {
                Some((sources, cut)) => {
                    let dest_for_scan = destination.clone();
                    let sources_for_scan = sources.clone();
                    let conflicts = tokio::task::spawn_blocking(move || {
                        detect_conflicts(&sources_for_scan, &dest_for_scan)
                    })
                    .await
                    .unwrap_or_default();
                    Some((sources, cut, destination, conflicts))
                }
                None => None,
            };
            let _ = tx.send(result);
        });

        cx.spawn(async move |this, cx| {
            let result = rx.await.ok().flatten();
            let _ = this.update(cx, |explorer, cx| {
                explorer.pending_conflict_check = false;
                let Some((sources, cut, destination, conflicts)) = result else {
                    explorer.status_message =
                        Some("The clipboard has no files to paste.".to_string());
                    cx.notify();
                    return;
                };
                let kind = if cut {
                    TransferKind::Move
                } else {
                    TransferKind::Copy
                };
                if conflicts.is_empty() {
                    explorer.dispatch_resolved_transfer(
                        ResolvedTransfer {
                            kind,
                            sources,
                            destination,
                            default_policy: ConflictPolicy::AutoRename,
                            per_destination: HashMap::new(),
                            from_cut_clipboard: cut,
                        },
                        cx,
                    );
                } else {
                    explorer.conflict_dialog = Some(ConflictDialogState::new(
                        kind,
                        sources,
                        destination,
                        conflicts,
                        cut,
                    ));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn initiate_transfer(
        &mut self,
        kind: TransferKind,
        sources: Vec<PathBuf>,
        destination: PathBuf,
        from_cut_clipboard: bool,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_in_recycle_bin("Editing recycled items", cx) {
            return;
        }
        if self.active_operation.is_some()
            || self.conflict_dialog.is_some()
            || self.pending_conflict_check
        {
            self.status_message = Some("A file operation is already in progress.".to_string());
            cx.notify();
            return;
        }
        if sources.is_empty() {
            return;
        }
        self.pending_conflict_check = true;
        let runtime = Arc::clone(&self.runtime);
        let sources_for_scan = sources.clone();
        let dest_for_scan = destination.clone();
        let (tx, rx) = oneshot::channel();

        runtime.spawn_blocking(move || {
            let conflicts = detect_conflicts(&sources_for_scan, &dest_for_scan);
            let _ = tx.send(conflicts);
        });

        cx.spawn(async move |this, cx| {
            let conflicts = rx.await.unwrap_or_default();
            let _ = this.update(cx, |explorer, cx| {
                explorer.pending_conflict_check = false;
                if conflicts.is_empty() {
                    explorer.dispatch_resolved_transfer(
                        ResolvedTransfer {
                            kind,
                            sources,
                            destination,
                            default_policy: ConflictPolicy::AutoRename,
                            per_destination: HashMap::new(),
                            from_cut_clipboard,
                        },
                        cx,
                    );
                } else {
                    explorer.conflict_dialog = Some(ConflictDialogState::new(
                        kind,
                        sources,
                        destination,
                        conflicts,
                        from_cut_clipboard,
                    ));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn toggle_conflict_apply_to_all(&mut self, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.conflict_dialog {
            dialog.toggle_apply_to_all();
            cx.notify();
        }
    }

    pub fn resolve_conflict(
        &mut self,
        policy: ConflictPolicy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mut dialog) = self.conflict_dialog.take() else {
            return;
        };
        match dialog.choose(policy) {
            ConflictOutcome::NextConflict => {
                self.conflict_dialog = Some(dialog);
                cx.notify();
            }
            ConflictOutcome::Dispatch(resolved) => {
                a11y::close_modal(self, ModalSurface::Conflict, window, cx);
                self.dispatch_resolved_transfer(resolved, cx);
            }
            ConflictOutcome::Cancelled => {
                a11y::close_modal(self, ModalSurface::Conflict, window, cx);
                self.status_message = Some(format!("{} cancelled.", dialog.kind.verb()));
                cx.notify();
            }
        }
    }

    pub fn cancel_conflict_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = self.conflict_dialog.take() {
            a11y::close_modal(self, ModalSurface::Conflict, window, cx);
            self.status_message = Some(format!("{} cancelled.", dialog.kind.verb()));
            cx.notify();
        }
    }

    fn activate_conflict_focused_slot(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.conflict_dialog else {
            return;
        };
        let multi = dialog.has_multiple_conflicts();
        let Some(slot) = a11y::modal_focused_slot(self, ModalSurface::Conflict, window) else {
            return;
        };
        let action_slot = if multi {
            if slot == 0 {
                self.toggle_conflict_apply_to_all(cx);
                return;
            }
            slot - 1
        } else {
            slot
        };
        match action_slot {
            0 => self.cancel_conflict_dialog(window, cx),
            1 => self.resolve_conflict(ConflictPolicy::AutoRename, window, cx),
            2 => self.resolve_conflict(ConflictPolicy::Skip, window, cx),
            3 => self.resolve_conflict(ConflictPolicy::Overwrite, window, cx),
            _ => {}
        }
    }

    fn dispatch_resolved_transfer(&mut self, transfer: ResolvedTransfer, cx: &mut Context<Self>) {
        let operation = match transfer.kind {
            TransferKind::Copy => FileOperation::CopyItems {
                sources: transfer.sources,
                destination: transfer.destination,
                conflict: transfer.default_policy,
                per_destination: transfer.per_destination,
            },
            TransferKind::Move => FileOperation::MoveItems {
                sources: transfer.sources,
                destination: transfer.destination,
                conflict: transfer.default_policy,
                per_destination: transfer.per_destination,
                from_cut_clipboard: transfer.from_cut_clipboard,
            },
        };
        self.start_operation(operation, cx);
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
        if let Some(op) = &mut self.active_operation {
            op.cancelling = true;
            op.cancel.cancel();
            self.status_message = Some("Cancelling file operation…".to_string());
            cx.notify();
        }
    }

    fn start_operation(&mut self, operation: FileOperation, cx: &mut Context<Self>) {
        let is_recycle_bin_op = operation.is_recycle_bin_operation();
        if !is_recycle_bin_op && self.refuse_in_recycle_bin("Editing recycled items", cx) {
            return;
        }
        if self.active_operation.is_some() {
            self.status_message = Some("A file operation is already in progress.".to_string());
            cx.notify();
            return;
        }
        let title = operation.title();
        let progress_title = operation.progress_title();
        let kind = operation.operation_kind();
        let is_undo = matches!(operation, FileOperation::Undo);
        let is_recycle_delete = matches!(operation, FileOperation::Recycle(_));
        let is_empty_recycle = matches!(operation, FileOperation::EmptyRecycleBin);
        let cancel = CancellationToken::new();
        self.status_message = None;
        self.active_operation = Some(ActiveOperation {
            title: progress_title,
            kind,
            current_item: String::new(),
            completed_items: 0,
            total_items: 0,
            bytes_done: 0,
            bytes_total: 0,
            error: None,
            cancelling: false,
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
                is_recycle_delete,
                is_recycle_bin_op,
                is_empty_recycle,
            });
        });

        cx.spawn(async move |this, cx| {
            while let Some(event) = event_rx.recv().await {
                let ok = this
                    .update(cx, |explorer, cx| match event {
                        OperationEvent::Progress(progress) => {
                            if let Some(active) = &mut explorer.active_operation {
                                active.title = operation_kind_title(progress.kind);
                                active.kind = Some(progress.kind);
                                active.current_item = progress.current_item;
                                active.completed_items = progress.completed_items;
                                active.total_items = progress.total_items;
                                active.bytes_done = progress.bytes_copied;
                                active.bytes_total = progress.total_bytes;
                                active.error = progress.error;
                            }
                            cx.notify();
                        }
                        OperationEvent::Finished {
                            result,
                            can_undo,
                            affected,
                            is_undo,
                            is_recycle_delete,
                            is_recycle_bin_op,
                            is_empty_recycle,
                        } => {
                            let succeeded = result.is_ok();
                            explorer.active_operation = None;
                            explorer.can_undo = can_undo;
                            explorer.status_message = Some(operation_message(title, result));
                            if is_undo {
                                explorer.directories.clear();
                                explorer.refresh(cx);
                            } else if is_recycle_bin_op {
                                if is_empty_recycle && succeeded {
                                    explorer.recycle_bin_has_items = false;
                                }
                                explorer.refresh_affected_paths(&affected, cx);
                                if explorer.is_recycle_bin_mode() {
                                    explorer.load_recycle_bin(false, cx);
                                } else {
                                    explorer.refresh_recycle_bin_icons(cx);
                                }
                            } else {
                                if is_recycle_delete && succeeded {
                                    explorer.recycle_bin_has_items = true;
                                    explorer.refresh_recycle_bin_icons(cx);
                                }
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
                self.load_secondary_directory(secondary, false, false, cx);
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
        &mut self,
        path: PathBuf,
        action: impl FnOnce(&Path) -> OnyxResult<()> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_in_recycle_bin("That Shell action", cx) {
            return;
        }
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.active_pane;
        if self.pane_shows_recycle_bin(pane) {
            let Some(entry) = self.pane_entries(pane).get(idx).cloned() else {
                return;
            };
            self.select_entry_in(pane, idx, cx);
            let can_restore = self.active_operation.is_none()
                && self
                    .recycle_items
                    .iter()
                    .find(|item| item.shell_path == entry.path)
                    .is_some_and(can_restore_recycle_item);
            self.context_menu = Some(ContextMenuState::new(
                ContextMenuTarget::RecycleItem {
                    index: idx,
                    can_restore,
                },
                f32::from(position.x),
                f32::from(position.y),
                false,
            ));
            a11y::open_modal(self, ModalSurface::ContextMenu, 0, window, cx);
            cx.notify();
            return;
        }
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
        a11y::open_modal(self, ModalSurface::ContextMenu, 0, window, cx);
        cx.notify();
    }

    pub fn open_background_context_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_recycle_bin_mode() {
            self.context_menu = Some(ContextMenuState::new(
                ContextMenuTarget::RecycleBackground {
                    has_items: self.can_empty_recycle_bin(),
                },
                f32::from(position.x),
                f32::from(position.y),
                false,
            ));
            a11y::open_modal(self, ModalSurface::ContextMenu, 0, window, cx);
            cx.notify();
            return;
        }
        self.context_menu = Some(ContextMenuState::new(
            ContextMenuTarget::Background {
                dir_path: self.active_directory(),
            },
            f32::from(position.x),
            f32::from(position.y),
            self.can_paste,
        ));
        a11y::open_modal(self, ModalSurface::ContextMenu, 0, window, cx);
        cx.notify();
    }

    pub fn close_context_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.context_menu.take().is_some() {
            a11y::close_modal(self, ModalSurface::ContextMenu, window, cx);
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
        a11y::close_modal(self, ModalSurface::ContextMenu, window, cx);
        cx.notify();

        match (action, state.target) {
            (ContextMenuAction::Open, ContextMenuTarget::Item { index, .. }) => {
                self.activate_entry(index, window, cx);
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
                self.initiate_paste(destination, cx);
            }
            (ContextMenuAction::Paste, ContextMenuTarget::Background { dir_path }) => {
                self.initiate_paste(dir_path, cx);
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
            (ContextMenuAction::Restore, ContextMenuTarget::RecycleItem { index, .. }) => {
                self.restore_recycle_item_at(index, cx);
            }
            (ContextMenuAction::EmptyRecycleBin, ContextMenuTarget::RecycleBackground { .. }) => {
                self.prompt_empty_recycle_bin(window, cx);
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
                    ContextMenuTarget::RecycleItem { .. }
                    | ContextMenuTarget::RecycleBackground { .. } => return,
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
                            explorer.load_secondary_directory(affected_dir, false, false, cx);
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
                    self.load_secondary_directory(current, false, false, cx);
                }
            }
        }
    }

    // ---- Archive browser & ZIP actions (Stage 3B) ----

    /// Opens the archive browser for `path` and lists its entries asynchronously.
    pub fn open_archive_browser(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        a11y::open_modal(self, ModalSurface::Archive, 1, window, cx);
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
            a11y::close_modal(self, ModalSurface::Archive, window, cx);
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

    /// Resets the workspace layout to the defaults (`reset-layout`), mirroring
    /// Slint's `Controller::reset_layout`.
    pub fn reset_layout(&mut self, cx: &mut Context<Self>) {
        self.sidebar_visible = true;
        self.preview_visible = true;
        self.sidebar_width = onyx_config::DEFAULT_SIDEBAR_WIDTH;
        self.preview_width = None;
        self.preview_side = PreviewSide::Right;
        self.pane_split_ratio = onyx_config::DEFAULT_PANE_SPLIT_RATIO;
        self.collapsed_sidebar_groups.clear();
        // A layout the user picked explicitly wins over a session that is still
        // being restored by the bootstrap task.
        self.layout_touched = true;
        self.persist_session();
        cx.notify();
    }

    /// Extracts the selected archive into `<active dir>/<stem>`.
    pub fn extract_selection_to_folder(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        if !entry_is_archive(entry) {
            self.status_message = Some("Select a .zip, .tar.gz or .tgz archive first.".to_string());
            cx.notify();
            return;
        }
        let archive_path = entry.path.clone();
        let dest_dir = extract_folder_for(&archive_path);
        self.start_operation(
            FileOperation::ExtractArchive {
                archive_path,
                dest_dir,
            },
            cx,
        );
    }

    /// Opens the archive browser for the selected archive.
    pub fn browse_selected_archive(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        if !entry_is_archive(entry) {
            self.status_message = Some("Select a .zip, .tar.gz or .tgz archive first.".to_string());
            cx.notify();
            return;
        }
        let path = entry.path.clone();
        self.open_archive_browser(path, window, cx);
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
        if self.refuse_in_recycle_bin("Dragging recycled items", cx) {
            return;
        }
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
                let kind = if drag.copy {
                    TransferKind::Copy
                } else {
                    TransferKind::Move
                };
                self.initiate_transfer(kind, vec![drag.source], destination, false, cx);
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

    // ---- External drag & drop (OLE `DoDragDrop` out of the window) ----

    /// Hands the armed drag over to the shell once the pointer has left the
    /// window, so items can be dropped into Explorer, the desktop or Outlook.
    ///
    /// Callers reach this through the raw window mouse listener of
    /// [`external_drag_watcher`], because GPUI's hit-tested `on_mouse_move`
    /// handlers stop firing at the window edge. The internal drag is dropped
    /// without touching the selection, so a cancelled drop behaves like `Escape`.
    pub(crate) fn hand_drag_to_shell(
        &mut self,
        pointer: (f32, f32),
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        log_drag(format!(
            "handover pointer=({:.0},{:.0}) viewport=({:.0},{:.0}) drag={} armed={} external={}",
            pointer.0,
            pointer.1,
            self.viewport_size.0,
            self.viewport_size.1,
            self.drag.is_some(),
            self.drag.as_ref().is_some_and(|drag| drag.armed),
            self.external_drag_active,
        ));
        if !drag_drop::should_start_external_drag(
            self.drag.as_ref(),
            self.external_drag_active,
            pointer,
            self.viewport_size,
        ) {
            return;
        }
        let Some(drag) = self.drag.take() else {
            return;
        };
        cx.notify();
        // The pointer already left the client area, so the drag image is anchored
        // at the point the drag started from: that is where the held item sits.
        let drag_image = drag_image_source(window, drag.origin);
        self.start_external_drag(drag.source, drag_image, cx);
    }

    /// Handles files dropped into Onyx from Windows Explorer, the Desktop, or another
    /// application via GPUI's `ExternalPaths` drag-and-drop pipeline (`.on_drop`),
    /// targeting the active directory as fallback when dropped outside a specific pane/folder.
    pub(crate) fn handle_external_paths_drop(
        &mut self,
        paths: &ExternalPaths,
        cx: &mut Context<Self>,
    ) {
        let destination = self.active_directory();
        self.handle_external_paths_drop_to(paths, destination, cx);
    }

    /// Handles files dropped into a specific destination folder (a pane's current directory,
    /// a hovered folder row/card, a tab, or a sidebar entry).
    pub(crate) fn handle_external_paths_drop_to(
        &mut self,
        paths: &ExternalPaths,
        destination: PathBuf,
        cx: &mut Context<Self>,
    ) {
        // When Onyx itself is the active OLE drag source, releasing back inside our own
        // window cancels cleanly instead of running a self-drop or recycling the file.
        if self.external_drag_active {
            self.external_drag_returned_inside = true;
            self.external_drop = None;
            cx.notify();
            return;
        }
        if Self::is_recycle_bin_path(&destination) {
            self.status_message =
                Some("Dropping files here is not available in Recycle Bin.".to_string());
            cx.notify();
            return;
        }
        let raw_paths = paths.paths();
        if raw_paths.is_empty() {
            return;
        }
        let mut valid_sources = Vec::with_capacity(raw_paths.len());
        let mut last_reject = None;
        for path in raw_paths {
            match validate_drop(path, &destination) {
                Ok(_) => valid_sources.push(path.clone()),
                Err(reject) => last_reject = Some(reject),
            }
        }
        if valid_sources.is_empty() {
            if let Some(reject) = last_reject {
                self.status_message = Some(reject.message().to_string());
                cx.notify();
            }
            return;
        }
        // Same volume moves, another volume copies — matching Windows Explorer's CF_HDROP rule.
        let same_volume = valid_sources
            .iter()
            .all(|path| path.components().next() == destination.components().next());
        let kind = if same_volume {
            TransferKind::Move
        } else {
            TransferKind::Copy
        };
        self.initiate_transfer(kind, valid_sources, destination, false, cx);
    }

    /// Files another application drags into the window: remember them on
    /// `Entered`, follow the pointer on `Pending` and run the operation on
    /// `Submit`.
    pub(crate) fn handle_file_drop(&mut self, event: &FileDropEvent, cx: &mut Context<Self>) {
        // When Onyx itself is the active OLE drag source, dragging the item back
        // into our own window and releasing must cancel cleanly rather than running
        // a self-drop operation or letting `finish_external_drag` recycle the file.
        if self.external_drag_active {
            if matches!(event, FileDropEvent::Submit { .. }) {
                self.external_drag_returned_inside = true;
                self.external_drop = None;
                cx.notify();
            }
            return;
        }
        match event {
            FileDropEvent::Entered { position, paths } => {
                let paths = paths.paths().to_vec();
                if paths.is_empty() {
                    return;
                }
                self.external_drop = Some(ExternalDropIn {
                    paths,
                    position: (f32::from(position.x), f32::from(position.y)),
                });
                self.status_message = Some("Drop the files to add them here.".to_string());
                cx.notify();
            }
            FileDropEvent::Pending { position } => {
                if let Some(drop) = self.external_drop.as_mut() {
                    drop.position = (f32::from(position.x), f32::from(position.y));
                    cx.notify();
                }
            }
            FileDropEvent::Submit { .. } => {
                let Some(drop) = self.external_drop.take() else {
                    return;
                };
                let destination = self.active_directory();
                // Same volume moves, another volume copies - the Windows rule the
                // shell applies for `CF_HDROP` drops.
                let same_volume = drop
                    .paths
                    .iter()
                    .all(|path| path.components().next() == destination.components().next());
                let kind = if same_volume {
                    TransferKind::Move
                } else {
                    TransferKind::Copy
                };
                self.initiate_transfer(kind, drop.paths, destination, false, cx);
            }
            FileDropEvent::Exited | FileDropEvent::Ended => {
                if self.external_drop.take().is_some() {
                    self.status_message = None;
                    cx.notify();
                }
            }
        }
    }

    /// Schedules the synchronous OLE `DoDragDrop` loop for one dragged item on the
    /// UI STA thread *outside* the active `MouseMoveEvent` / `Entity::update` borrow.
    ///
    /// Running `DoDragDrop` directly inside `Entity::update` leaves `App` and
    /// `OnyxExplorer` mutably borrowed while OLE pumps a nested Win32 message loop;
    /// dragging the item back over Onyx then re-enters GPUI's `IDropTarget` and
    /// panics with `BorrowMutError`. Deferring via `cx.spawn` runs `DoDragDrop` on
    /// the same UI STA thread (so `ReleaseCapture` and mouse capture work) with zero
    /// `App` borrows held.
    fn start_external_drag(
        &mut self,
        source: PathBuf,
        drag_image: Option<DragImageSource>,
        cx: &mut Context<Self>,
    ) {
        if self.external_drag_active {
            return;
        }
        self.external_drag_active = true;
        self.external_drag_returned_inside = false;
        self.status_message = Some("Drag the selection into another application.".to_string());
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = start_external_file_drag(std::slice::from_ref(&source), drag_image)
                .map_err(|err| err.to_string());
            let _ = this.update(cx, |explorer, cx| {
                explorer.finish_external_drag(result, &source, cx);
            });
        })
        .detach();
    }

    /// Reports the OLE outcome and keeps the panes in sync with the shell.
    fn finish_external_drag(
        &mut self,
        result: Result<DragOutcome, String>,
        source: &Path,
        cx: &mut Context<Self>,
    ) {
        self.external_drag_active = false;
        let returned_inside = std::mem::take(&mut self.external_drag_returned_inside);
        let result = if returned_inside {
            Ok(DragOutcome::Cancelled)
        } else {
            result
        };
        log_drag(format!(
            "finished source={} outcome={:?}",
            source.display(),
            result.as_ref().map(|outcome| format!("{outcome:?}"))
        ));
        let mut affected = Vec::new();
        add_parent(&mut affected, source);

        let message = match result {
            Ok(DragOutcome::Moved) => {
                // A `CF_HDROP` target performs the move itself, so the source is
                // normally gone already. A source that survived was only copied by
                // the target and is removed through the operations engine, which
                // never deletes permanently.
                if source.exists() {
                    self.start_operation(FileOperation::Recycle(source.to_path_buf()), cx);
                } else {
                    self.refresh_affected_paths(&affected, cx);
                }
                "Dragged selection moved.".to_string()
            }
            Ok(DragOutcome::Copied) => "Dragged selection copied.".to_string(),
            Ok(DragOutcome::Cancelled) => "External drag cancelled.".to_string(),
            Err(message) => format!("External drag failed: {message}"),
        };
        self.status_message = Some(message);
        cx.notify();
    }

    // ---- Command Palette & Settings overlays (Stage 3A) ----

    /// Reports whether a blocking overlay owns the keyboard right now.
    #[must_use]
    /// Modal surface that currently owns the keyboard, in key-routing order.
    pub(crate) fn active_modal_surface(&self) -> Option<ModalSurface> {
        if self.context_menu.is_some() {
            Some(ModalSurface::ContextMenu)
        } else if self.conflict_dialog.is_some() {
            Some(ModalSurface::Conflict)
        } else if self.prompt_state.is_some() {
            Some(ModalSurface::Prompt)
        } else if self.command_palette.is_some() {
            Some(ModalSurface::Palette)
        } else if self.archive.is_some() {
            Some(ModalSurface::Archive)
        } else if self.settings_open {
            Some(ModalSurface::Settings)
        } else if self.quick_look_open {
            Some(ModalSurface::QuickLook)
        } else {
            None
        }
    }

    pub fn overlay_open(&self) -> bool {
        self.command_palette.is_some()
            || self.settings_open
            || self.archive_is_open()
            || self.conflict_dialog.is_some()
    }

    pub fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.context_menu = None;
        self.prompt_state = None;
        self.settings_open = false;
        self.quick_look_open = false;
        a11y::remember_modal_focus(self, ModalSurface::Palette, window, cx);
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
            a11y::close_modal(self, ModalSurface::Palette, window, cx);
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
        palette.search.hits.clear();
        palette.search.selected = 0;
        let root = self.current_path().to_path_buf();
        let engine = self.search_engine.clone();
        let query = SearchQuery {
            text,
            include_hidden: self.show_hidden,
            use_index: self.search_index.is_some(),
            ..SearchQuery::default()
        };
        let (tx, mut rx) = mpsc::channel(search_index::STREAM_EVENT_CHANNEL_CAPACITY);
        let worker_cancel = cancel.clone();
        self.runtime.spawn(async move {
            search_index::run_query_streaming(
                engine,
                generation,
                root,
                query,
                worker_cancel,
                true,
                tx,
            )
            .await;
        });
        cx.spawn(async move |this, cx| {
            while let Some(event) = rx.recv().await {
                let keep_going = this
                    .update(cx, |explorer, cx| {
                        explorer.handle_palette_search_event(event, cx)
                    })
                    .unwrap_or(false);
                if !keep_going {
                    cancel.cancel();
                    break;
                }
            }
        })
        .detach();
        self.ensure_led_animation(cx);
        cx.notify();
    }

    fn handle_palette_search_event(
        &mut self,
        event: SearchStreamEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let show_system = self.config.show_system;
        let Some(palette) = self.command_palette.as_mut() else {
            return false;
        };
        match event {
            SearchStreamEvent::Batch {
                generation,
                results,
            } => {
                let Some(hits) = palette.search.run.accept_batch(generation, results) else {
                    return false;
                };
                let filtered: Vec<FileEntry> = hits
                    .into_iter()
                    .filter(|entry| show_system || !entry.attributes.system)
                    .collect();
                if !filtered.is_empty() {
                    palette.search.append_hits(filtered);
                }
                cx.notify();
                true
            }
            SearchStreamEvent::Finished {
                generation,
                outcome,
            } => {
                if !palette.search.run.finish_stream(generation, outcome) {
                    return false;
                }
                cx.notify();
                false
            }
        }
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
            "move-tab-pane" => self.move_tab_to_other_pane(None, cx),
            "refresh" => self.refresh(cx),
            "up" => self.go_up(cx),
            "favorite" => self.toggle_favorite(cx),
            "quick-look" => self.toggle_quick_look(window, cx),
            "theme" => self.toggle_theme(window, cx),
            "view-mode" => self.toggle_view_mode(cx),
            "group-by" => self.cycle_group_by(cx),
            "hidden" => self.set_show_hidden(!self.show_hidden, cx),
            "system-files" => self.set_show_system(!self.config.show_system, cx),
            "sidebar" => self.toggle_sidebar(cx),
            "dual" => self.toggle_dual_pane(cx),
            "swap-panes" => self.swap_panes(cx),
            "preview" => self.set_preview_visible(!self.preview_visible, cx),
            "preview-side" => self.toggle_preview_side(cx),
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
            "reset-layout" => self.reset_layout(cx),
            "compress-zip" => self.compress_selection_to_zip(cx),
            "extract-here" => self.extract_selected_archive(cx),
            "extract-to-folder" => self.extract_selection_to_folder(cx),
            "browse-archive" => self.browse_selected_archive(window, cx),
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
        // Start on the first settings control; the header close affordances are
        // reachable with Shift+Tab.
        a11y::open_modal(self, ModalSurface::Settings, 2, window, cx);
        cx.notify();
    }

    pub fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_open {
            self.settings_open = false;
            a11y::close_modal(self, ModalSurface::Settings, window, cx);
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
        self.request_index_build(self.index_roots(), cx);
        cx.notify();
    }

    fn reload_panes_after_listing_change(&mut self, cx: &mut Context<Self>) {
        let path = self.current_path().to_path_buf();
        self.directories.invalidate(&path);
        self.load_directory(path, false, false, cx);
        if self.dual_pane {
            let secondary = self.secondary.path.clone();
            self.directories.invalidate(&secondary);
            self.load_secondary_directory(secondary, false, false, cx);
        }
    }

    // ---- Global Keyboard Shortcuts ----

    fn handle_key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = ev.keystroke.key.as_str();
        let mods = &ev.keystroke.modifiers;

        // Modal surfaces own the keyboard: Tab walks their stops in reading order
        // and wraps there, so it can never reach the panes behind the surface.
        if let Some(surface) = self.active_modal_surface()
            && key == "tab"
        {
            match surface {
                ModalSurface::Palette if a11y::modal_slot_is_focused(self, surface, 0, window) => {
                    self.palette_input
                        .update(cx, |input, cx| input.focus(window, cx));
                }
                ModalSurface::Prompt => self.step_prompt_focus(!mods.shift, window, cx),
                _ => a11y::step_modal_focus(self, surface, !mods.shift, window, cx),
            }
            cx.stop_propagation();
            return;
        }

        if let Some(menu) = &mut self.context_menu {
            match key {
                "escape" => {
                    self.close_context_menu(window, cx);
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

        if self.conflict_dialog.is_some() {
            match key {
                "escape" => {
                    self.cancel_conflict_dialog(window, cx);
                    cx.stop_propagation();
                }
                "enter" | "space" => {
                    self.activate_conflict_focused_slot(window, cx);
                    cx.stop_propagation();
                }
                _ => {}
            }
            return;
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
            // Enter runs the highlighted command while the result list itself
            // owns the keyboard; the query field handles Enter through its own
            // submit listener.
            let list_focused = a11y::modal_slot_is_focused(self, ModalSurface::Palette, 0, window);
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
                "enter" if list_focused => {
                    // Both modes: commands run, file hits open. Going through
                    // `palette_selected_id` alone dropped Enter in search mode.
                    self.activate_palette_selection(window, cx);
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

        // In Recycle Bin mode, navigation keys move selection, Enter restores the
        // selected item, and Shift+Delete opens the Empty Recycle Bin confirmation modal.
        if self.is_recycle_bin_mode() {
            match key {
                "up" | "left" => self.move_selection(-1, cx),
                "down" | "right" => self.move_selection(1, cx),
                "backspace" => self.go_up(cx),
                "enter" => {
                    if let Some(idx) = self.pane_selected_index(self.active_pane) {
                        self.activate_entry(idx, window, cx);
                    }
                }
                "delete" if mods.shift => {
                    self.prompt_empty_recycle_bin(window, cx);
                }
                _ => {
                    if let Some(ch) = type_ahead_char(ev)
                        && self.handle_type_ahead(ch, cx)
                    {
                        cx.stop_propagation();
                    }
                }
            }
            return;
        }

        // Quick Look modal captures Space, Escape, Enter, and Arrow keys
        if self.quick_look_open {
            match key {
                "escape" | "space" | " " => {
                    self.close_quick_look(window, cx);
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
                    self.open_selected_from_quick_look(window, cx);
                    cx.stop_propagation();
                }
                _ => {}
            }
            return;
        }

        if !mods.control && !mods.alt && (key == "space" || key == " ") {
            self.toggle_quick_look(window, cx);
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
        if mods.control && mods.shift && key == "m" {
            self.move_tab_to_other_pane(None, cx);
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
                        self.activate_entry(idx, window, cx);
                    }
                }
                "backspace" => self.go_up(cx),
                _ => {
                    if let Some(ch) = type_ahead_char(ev)
                        && self.handle_type_ahead(ch, cx)
                    {
                        cx.stop_propagation();
                    }
                }
            }
        } else {
            match key {
                // Slint moves the selection with the horizontal arrows too in the
                // details view (`main.slint`); history stays on Alt+Left/Right.
                "up" | "left" => self.move_selection(-1, cx),
                "down" | "right" => self.move_selection(1, cx),
                "enter" => {
                    if let Some(idx) = self.pane_selected_index(self.active_pane) {
                        self.activate_entry(idx, window, cx);
                    }
                }
                "backspace" => self.go_up(cx),
                _ => {
                    if let Some(ch) = type_ahead_char(ev)
                        && self.handle_type_ahead(ch, cx)
                    {
                        cx.stop_propagation();
                    }
                }
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
        if self.preview_side == PreviewSide::Left {
            if let Some(preview_el) = preview_pane::render_preview_pane(self, layout.preview_w, cx)
            {
                workspace = workspace.child(preview_el);
            }
            if self.preview_visible {
                workspace = workspace.child(splitter::render_splitter(
                    SplitterKind::Preview,
                    dragging_kind == Some(SplitterKind::Preview),
                    theme,
                    cx,
                ));
            }
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
        if self.preview_side == PreviewSide::Right {
            if self.preview_visible {
                workspace = workspace.child(splitter::render_splitter(
                    SplitterKind::Preview,
                    dragging_kind == Some(SplitterKind::Preview),
                    theme,
                    cx,
                ));
            }
            if let Some(preview_el) = preview_pane::render_preview_pane(self, layout.preview_w, cx)
            {
                workspace = workspace.child(preview_el);
            }
        }

        let mut root = div()
            .id("onyx-root")
            .role(Role::Application)
            .aria_label("Onyx")
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
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|explorer, _: &MouseDownEvent, _, cx| explorer.go_back(cx)),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Forward),
                cx.listener(|explorer, _: &MouseDownEvent, _, cx| explorer.go_forward(cx)),
            )
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
                    } else if !drag_drop::pointer_left_window(pointer, explorer.viewport_size) {
                        // Windows drops the button state once the pointer is
                        // outside the client area, so a missing button only ends
                        // the gesture inside the window; outside, the drag has to
                        // survive so the raw listener can hand it to the shell.
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
            .on_drop(cx.listener(|explorer, paths: &ExternalPaths, _window, cx| {
                explorer.handle_external_paths_drop(paths, cx);
            }))
            .child(tab_bar::render_tab_bar(self, window, cx))
            .child(nav_bar::render_nav_bar(self, window, cx))
            .child(toolbar::render_toolbar(self, window, cx))
            .child(workspace)
            .child(status_bar::render_status_bar(self, cx))
            // Only there to register the pointer listener that turns a drag
            // leaving the window into a native OLE drag. It must have bounds
            // (GPUI does not paint a zero-sized element, and the listener is
            // registered in the paint callback), so it is a one-pixel strip along
            // the top edge that no interactive element relies on.
            .child(
                div()
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .w_full()
                    .h(px(1.0))
                    .child(external_drag_watcher(cx)),
            );

        if let Some(suggestions_overlay) = nav_bar::render_address_suggestions_overlay(self, cx) {
            root = root.child(suggestions_overlay);
        }
        if let Some(quick_look_overlay) = preview_pane::render_quick_look_modal(self, cx) {
            root = root.child(quick_look_overlay);
        }
        if let Some(dialog) = self.conflict_dialog.as_mut()
            && dialog.needs_focus
        {
            dialog.needs_focus = false;
            let initial_slot = usize::from(dialog.has_multiple_conflicts());
            a11y::open_modal(self, ModalSurface::Conflict, initial_slot, window, cx);
        }

        if let Some(progress_toast) = overlays::render_operation_progress_toast(self, cx) {
            root = root.child(progress_toast);
        }
        if let Some(prompt_dialog) = overlays::render_prompt_dialog(self, cx) {
            root = root.child(prompt_dialog);
        }
        if let Some(conflict_dialog) = overlays::render_conflict_dialog(self, cx) {
            root = root.child(conflict_dialog);
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

/// Registers a raw window mouse listener that hands an armed drag to the shell
/// as soon as the pointer leaves the window.
///
/// `Window::on_mouse_event` is not gated by hit testing, so it still sees the
/// out-of-bounds positions Windows reports while the left button is held; the
/// hit-tested `on_mouse_move` handlers of the panes do not.
fn external_drag_watcher(cx: &mut Context<OnyxExplorer>) -> impl IntoElement {
    let view = cx.weak_entity();
    canvas(
        |_, _, _| (),
        move |_, (), window, _cx| {
            let drag_view = view.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let pointer = (f32::from(event.position.x), f32::from(event.position.y));
                let _ = drag_view.update(cx, |explorer, cx| {
                    explorer.hand_drag_to_shell(pointer, window, cx)
                });
            });
            window.on_mouse_event(move |event: &FileDropEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let _ = view.update(cx, |explorer, cx| explorer.handle_file_drop(event, cx));
            });
        },
    )
}

/// Appends a line to `%APPDATA%\onyx\logs\drag-debug.log` when `ONYX_DEBUG_DRAG`
/// is set.
///
/// Handing a drag to the shell depends on platform behaviour no unit test can
/// reach (capture, out-of-bounds coordinates, OLE apartment), so the path stays
/// inspectable on a real window without logging in normal operation.
fn log_drag(message: impl AsRef<str>) {
    if std::env::var_os("ONYX_DEBUG_DRAG").is_none() {
        return;
    }
    let Some(dir) = dirs::data_dir().map(|dir| dir.join("onyx").join("logs")) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("drag-debug.log"))
    else {
        return;
    };
    use std::io::Write as _;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    let _ = writeln!(file, "{stamp} {}", message.as_ref());
}

fn extract_window_hwnd(window: &Window) -> Option<isize> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(win32) => Some(win32.hwnd.get()),
        _ => None,
    }
}

/// Turns a viewport point into the window handle plus client-area point Windows
/// snapshots into the drag image of an external drag.
fn drag_image_source(window: &Window, point: (f32, f32)) -> Option<DragImageSource> {
    let hwnd = extract_window_hwnd(window)?;
    Some(DragImageSource::new(
        hwnd,
        point.0.round() as i32,
        point.1.round() as i32,
    ))
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

/// Builds the list row for one Recycle Bin item.
///
/// The row keeps the item's Shell path as its identity — unique per item and usable
/// as the icon cache key — reports the deletion time in the date column, and shows
/// the folder the item is restored to in the type column.
fn recycle_bin_row(item: &RecycleBinItem) -> FileEntry {
    let extension = Path::new(&item.name)
        .extension()
        .map(|ext| ext.to_string_lossy().into_owned())
        .filter(|ext| !ext.is_empty());
    FileEntry {
        path: item.shell_path.clone(),
        name: item.name.clone(),
        extension,
        kind: if item.is_dir {
            FileKind::Directory
        } else {
            FileKind::File
        },
        size: item.size,
        modified: item.deleted_at,
        created: None,
        attributes: FileAttributes::default(),
        is_symlink: false,
        symlink_target: None,
        type_label: item
            .original_path
            .as_deref()
            .and_then(Path::parent)
            .map_or_else(String::new, normalize_display_path),
    }
}

/// Filters `all_entries` by `needle`, overlays cached folder sizes and sorts the
/// result for one pane.
fn build_view_entries(
    all_entries: &[FileEntry],
    sort_spec: SortSpec,
    needle: &str,
    folder_sizes: &FolderSizeCache,
) -> Vec<FileEntry> {
    let query = needle.trim();
    if query.is_empty() {
        let mut entries = all_entries.to_vec();
        for entry in &mut entries {
            if entry.is_dir()
                && let Some(cached) = folder_sizes.get(&entry.path)
            {
                entry.size = cached.total_bytes;
            }
        }
        sort_entries(&mut entries, sort_spec);
        return entries;
    }

    // Inline fuzzy filter: matches filename or type label using `onyx_core::fuzzy_match`
    // and ranks stronger hits (exact / prefix / CamelCase / word-boundary) first.
    let mut scored: Vec<(i64, FileEntry)> = all_entries
        .iter()
        .filter_map(|entry| {
            let name_score = fuzzy_match(query, &entry.name).map(|m| m.score);
            let type_score = fuzzy_match(query, &entry.type_label).map(|m| m.score - 20);
            let best = match (name_score, type_score) {
                (Some(a), Some(b)) => a.max(b),
                (Some(a), None) => a,
                (None, Some(b)) => b,
                (None, None) => return None,
            };
            let mut cloned = entry.clone();
            if cloned.is_dir()
                && let Some(cached) = folder_sizes.get(&cloned.path)
            {
                cloned.size = cached.total_bytes;
            }
            Some((best, cloned))
        })
        .collect();

    scored.sort_by(|(score_a, entry_a), (score_b, entry_b)| {
        score_b
            .cmp(score_a)
            .then_with(|| natural_cmp(&entry_a.name, &entry_b.name))
    });
    scored.into_iter().map(|(_, entry)| entry).collect()
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

fn can_restore_recycle_item(item: &RecycleBinItem) -> bool {
    !item.shell_path.as_os_str().is_empty()
        && item
            .original_path
            .as_deref()
            .and_then(Path::parent)
            .is_some_and(|parent| !parent.as_os_str().is_empty())
}

fn empty_recycle_bin_prompt_message(item_count: usize) -> String {
    if item_count == 1 {
        "Permanently delete 1 item in the Recycle Bin? This cannot be undone.".to_string()
    } else {
        format!("Permanently delete {item_count} items in the Recycle Bin? This cannot be undone.")
    }
}

fn operation_message(title: &str, result: OnyxResult<Option<OperationReport>>) -> String {
    match result {
        Ok(None) => "Nothing to undo.".into(),
        Ok(Some(report)) if report.failed.is_empty() && report.skipped.is_empty() => {
            if report.succeeded.is_empty() {
                format!("{title} complete.")
            } else {
                format!("{title} complete · {} items", report.succeeded.len())
            }
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

/// Transfers a tab between the primary [`TabManager`] and the secondary [`TabState`]
/// while preserving `path`, `history`, `sort`, and `selected_paths`.
///
/// Mirrors `crates/onyx-app/src/state.rs::AppState::move_tab_to_other_pane`.
fn transfer_tab_between_panes(
    tabs: &mut TabManager,
    secondary_tab: &mut TabState,
    active_pane: &mut PaneId,
    primary_tab_index: Option<usize>,
) -> bool {
    let source_primary_index = match primary_tab_index {
        Some(idx) => Some(idx),
        None if *active_pane == PaneId::Primary => Some(tabs.active_index()),
        None => None,
    };

    if let Some(index) = source_primary_index {
        if index >= tabs.tabs().len() {
            return false;
        }
        if tabs.tabs().len() <= 1 {
            let Some(primary_tab) = tabs.active_tab_mut() else {
                return false;
            };
            let primary_id = primary_tab.id;
            std::mem::swap(primary_tab, secondary_tab);
            primary_tab.id = primary_id;
            *active_pane = PaneId::Secondary;
            return true;
        }

        let Some(moved) = tabs.tabs().get(index).cloned() else {
            return false;
        };
        if !tabs.close_tab(index) {
            return false;
        }
        *secondary_tab = moved;
        *active_pane = PaneId::Secondary;
        true
    } else {
        let moved = secondary_tab.clone();
        tabs.new_tab(moved.path.clone());
        if let Some(dst) = tabs.active_tab_mut() {
            let new_id = dst.id;
            *dst = moved;
            dst.id = new_id;
        }
        *active_pane = PaneId::Primary;
        true
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

    fn test_recycle_item(name: &str, is_dir: bool) -> RecycleBinItem {
        RecycleBinItem {
            name: name.to_string(),
            shell_path: PathBuf::from(format!(r"C:\$Recycle.Bin\S-1-5-21\$RABCDEF.{name}")),
            original_path: Some(PathBuf::from(format!(r"C:\Users\me\Desktop\{name}"))),
            size: 4096,
            deleted_at: Some(1_700_000_000),
            is_dir,
        }
    }

    #[test]
    fn recycle_bin_row_maps_the_shell_item() {
        let row = recycle_bin_row(&test_recycle_item("report.txt", false));
        assert_eq!(row.name, "report.txt");
        assert_eq!(row.extension.as_deref(), Some("txt"));
        assert_eq!(row.kind, FileKind::File);
        assert!(!row.is_dir());
        assert_eq!(row.size, 4096);
        // The date column shows when the item was deleted.
        assert_eq!(row.modified, Some(1_700_000_000));
        // The type column shows the folder the item is restored to.
        assert_eq!(row.type_label, r"C:\Users\me\Desktop");
        assert!(
            row.path.to_string_lossy().contains("$RABCDEF"),
            "the shell path identifies the item"
        );
    }

    #[test]
    fn recycle_bin_row_keeps_deleted_folders_folders() {
        let row = recycle_bin_row(&test_recycle_item("photos", true));
        assert_eq!(row.kind, FileKind::Directory);
        assert!(row.is_dir());
        assert_eq!(row.extension, None);
    }

    #[test]
    fn recycle_bin_row_survives_a_missing_original_location() {
        let mut item = test_recycle_item("report.txt", false);
        item.original_path = None;
        let row = recycle_bin_row(&item);
        assert_eq!(row.type_label, "");
        assert_eq!(row.name, "report.txt");
    }

    #[test]
    fn the_recycle_bin_namespace_path_routes_to_the_shell_listing() {
        let namespace = OnyxExplorer::recycle_bin_path();
        assert!(OnyxExplorer::is_recycle_bin_path(&namespace));
        assert!(OnyxExplorer::is_recycle_bin_path(&PathBuf::from(
            r"::{645ff040-5081-101b-9f08-00aa002f954e}"
        )));
        assert!(!OnyxExplorer::is_recycle_bin_path(Path::new(r"C:\Users")));
        assert!(!OnyxExplorer::is_recycle_bin_path(Path::new(
            r"C:\$Recycle.Bin"
        )));
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
        assert_eq!(
            operation_message("Empty Recycle Bin", Ok(Some(OperationReport::default()))),
            "Empty Recycle Bin complete."
        );
    }

    #[test]
    fn recycle_bin_restore_and_empty_state_helpers() {
        let valid = test_recycle_item("report.txt", false);
        assert!(can_restore_recycle_item(&valid));

        let mut missing_origin = valid.clone();
        missing_origin.original_path = None;
        assert!(!can_restore_recycle_item(&missing_origin));

        let mut empty_shell = valid;
        empty_shell.shell_path = PathBuf::new();
        assert!(!can_restore_recycle_item(&empty_shell));

        assert!(PromptKind::EmptyRecycleBin.is_danger());
        assert!(PromptKind::PermanentDelete(PathBuf::from("a.txt")).is_danger());
        assert!(!PromptKind::NewFolder(PathBuf::from("C:\\Temp")).is_danger());
        assert!(!PromptKind::Rename(PathBuf::from("C:\\Temp\\a.txt")).is_danger());

        assert!(FileOperation::EmptyRecycleBin.is_recycle_bin_operation());
        assert!(FileOperation::RestoreRecycleItems(Vec::new()).is_recycle_bin_operation());
        assert!(!FileOperation::Undo.is_recycle_bin_operation());

        assert_eq!(
            empty_recycle_bin_prompt_message(1),
            "Permanently delete 1 item in the Recycle Bin? This cannot be undone."
        );
        assert_eq!(
            empty_recycle_bin_prompt_message(4),
            "Permanently delete 4 items in the Recycle Bin? This cannot be undone."
        );
    }

    #[test]
    fn transfer_primary_tab_to_secondary_preserves_state() {
        let mut tabs = TabManager::new(PathBuf::from(r"C:\Alpha"));
        tabs.new_tab(PathBuf::from(r"C:\Beta"));
        if let Some(tab) = tabs.active_tab_mut() {
            tab.navigate(PathBuf::from(r"C:\Beta\Sub"), true);
            tab.sort = SortSpec {
                column: SortColumn::Size,
                direction: SortDirection::Descending,
                folders_first: true,
            };
            tab.selected_paths = vec![PathBuf::from(r"C:\Beta\Sub\notes.txt")];
        }
        let mut secondary = TabState::new(0, PathBuf::from(r"C:\Secondary"));
        let mut active_pane = PaneId::Primary;

        assert!(transfer_tab_between_panes(
            &mut tabs,
            &mut secondary,
            &mut active_pane,
            None,
        ));
        assert_eq!(active_pane, PaneId::Secondary);
        assert_eq!(tabs.tabs().len(), 1);
        assert_eq!(
            tabs.active_tab().map(|t| t.path.as_path()),
            Some(Path::new(r"C:\Alpha"))
        );
        assert_eq!(secondary.path, PathBuf::from(r"C:\Beta\Sub"));
        assert!(secondary.history.can_go_back());
        assert_eq!(
            secondary.sort,
            SortSpec {
                column: SortColumn::Size,
                direction: SortDirection::Descending,
                folders_first: true,
            }
        );
        assert_eq!(
            secondary.selected_paths,
            vec![PathBuf::from(r"C:\Beta\Sub\notes.txt")]
        );
    }

    #[test]
    fn transfer_sole_primary_tab_swaps_with_secondary_and_preserves_both() {
        let mut tabs = TabManager::new(PathBuf::from(r"C:\Primary"));
        let primary_id = tabs.active_tab().map(|t| t.id).unwrap_or(0);
        if let Some(tab) = tabs.active_tab_mut() {
            tab.navigate(PathBuf::from(r"C:\Primary\Deep"), true);
            tab.sort = SortSpec {
                column: SortColumn::Modified,
                direction: SortDirection::Descending,
                folders_first: true,
            };
            tab.selected_paths = vec![PathBuf::from(r"C:\Primary\Deep\a.txt")];
        }
        let mut secondary = TabState::new(0, PathBuf::from(r"C:\Secondary"));
        secondary.navigate(PathBuf::from(r"C:\Secondary\Child"), true);
        secondary.sort = SortSpec {
            column: SortColumn::Kind,
            direction: SortDirection::Ascending,
            folders_first: true,
        };
        secondary.selected_paths = vec![PathBuf::from(r"C:\Secondary\Child\b.txt")];

        let mut active_pane = PaneId::Primary;
        assert!(transfer_tab_between_panes(
            &mut tabs,
            &mut secondary,
            &mut active_pane,
            None,
        ));

        assert_eq!(active_pane, PaneId::Secondary);
        assert_eq!(tabs.tabs().len(), 1);
        let Some(new_primary) = tabs.active_tab() else {
            panic!("primary tab remains");
        };
        assert_eq!(new_primary.id, primary_id);
        assert_eq!(new_primary.path, PathBuf::from(r"C:\Secondary\Child"));
        assert!(new_primary.history.can_go_back());
        assert_eq!(new_primary.sort.column, SortColumn::Kind);
        assert_eq!(
            new_primary.selected_paths,
            vec![PathBuf::from(r"C:\Secondary\Child\b.txt")]
        );

        assert_eq!(secondary.path, PathBuf::from(r"C:\Primary\Deep"));
        assert!(secondary.history.can_go_back());
        assert_eq!(secondary.sort.column, SortColumn::Modified);
        assert_eq!(
            secondary.selected_paths,
            vec![PathBuf::from(r"C:\Primary\Deep\a.txt")]
        );
    }

    #[test]
    fn transfer_secondary_tab_to_primary_opens_new_active_tab_with_state() {
        let mut tabs = TabManager::new(PathBuf::from(r"C:\Primary"));
        let mut secondary = TabState::new(0, PathBuf::from(r"C:\Secondary"));
        secondary.navigate(PathBuf::from(r"C:\Secondary\Logs"), true);
        secondary.sort = SortSpec {
            column: SortColumn::Size,
            direction: SortDirection::Descending,
            folders_first: true,
        };
        secondary.selected_paths = vec![PathBuf::from(r"C:\Secondary\Logs\app.log")];

        let mut active_pane = PaneId::Secondary;
        assert!(transfer_tab_between_panes(
            &mut tabs,
            &mut secondary,
            &mut active_pane,
            None,
        ));

        assert_eq!(active_pane, PaneId::Primary);
        assert_eq!(tabs.tabs().len(), 2);
        let Some(moved) = tabs.active_tab() else {
            panic!("new active tab exists");
        };
        assert_ne!(moved.id, 0);
        assert_eq!(moved.path, PathBuf::from(r"C:\Secondary\Logs"));
        assert!(moved.history.can_go_back());
        assert_eq!(
            moved.sort,
            SortSpec {
                column: SortColumn::Size,
                direction: SortDirection::Descending,
                folders_first: true,
            }
        );
        assert_eq!(
            moved.selected_paths,
            vec![PathBuf::from(r"C:\Secondary\Logs\app.log")]
        );

        // Out-of-bounds index is a safe no-op.
        assert!(!transfer_tab_between_panes(
            &mut tabs,
            &mut secondary,
            &mut active_pane,
            Some(99),
        ));
    }

    #[test]
    fn type_ahead_matches_prefix_cycles_and_handles_unicode() {
        let make_dir = |name: &str| FileEntry {
            path: PathBuf::from(format!(r"C:\Test\{name}")),
            name: name.to_string(),
            extension: None,
            kind: FileKind::Directory,
            size: 0,
            modified: None,
            created: None,
            attributes: FileAttributes::default(),
            is_symlink: false,
            symlink_target: None,
            type_label: "Folder".to_string(),
        };
        let entries = vec![
            make_dir("Assets"),
            make_dir("Desktop"),
            make_dir("Documents"),
            make_dir("Downloads"),
            make_dir("Übungen"),
        ];

        // Fresh single-letter press jumps to the first matching folder.
        assert_eq!(find_type_ahead_match(&entries, "d", None, false), Some(1));
        // Repeated single-letter press cycles through Desktop -> Documents -> Downloads -> Desktop.
        assert_eq!(find_type_ahead_match(&entries, "d", Some(1), true), Some(2));
        assert_eq!(find_type_ahead_match(&entries, "d", Some(2), true), Some(3));
        assert_eq!(find_type_ahead_match(&entries, "d", Some(3), true), Some(1));
        // Multi-character prefix narrows directly to Downloads.
        assert_eq!(
            find_type_ahead_match(&entries, "dow", Some(1), false),
            Some(3)
        );
        // Case-insensitive Unicode (German umlaut).
        assert_eq!(
            find_type_ahead_match(&entries, "ü", Some(0), false),
            Some(4)
        );
        // Non-matching prefix returns None.
        assert_eq!(find_type_ahead_match(&entries, "z", Some(0), false), None);
    }
}
