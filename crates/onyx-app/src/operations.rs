use onyx_core::{OnyxError, OnyxResult, PaneId, format_bytes};
use onyx_operations::{
    ArchiveEntry, ConflictPolicy, FileOperationsEngine, OperationProgress, OperationReport,
    PermanentDeleteRequest, UndoAction, create_archive, extract_archive, generate_auto_rename_path,
    list_archive_entries,
};
use onyx_ui::{OnyxWindow, UiOperationProgress};
use onyx_windows::{DragOutcome, DropEvent, TerminalProfile};
use slint::ModelRc;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::controller::{Controller, post};
use crate::state::is_zip_path;
use crate::ui_sync;

type FileList = (Vec<PathBuf>, bool);

pub(crate) enum ClipboardCommand {
    Write(FileList),
    Read(oneshot::Sender<Option<FileList>>),
    Moved {
        original: Vec<PathBuf>,
        moved: Vec<PathBuf>,
    },
}

#[derive(Default)]
struct Clipboard {
    observed: Option<FileList>,
    fallback: Option<FileList>,
    consumed: Vec<PathBuf>,
}

impl Clipboard {
    fn read(&mut self) -> Option<FileList> {
        let system = onyx_windows::get_clipboard_files();
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
            paths.retain(|path| {
                !self
                    .consumed
                    .iter()
                    .any(|moved| ui_sync::same_path(moved, path))
            });
        }
        (!paths.is_empty()).then_some((paths, cut))
    }

    fn write(&mut self, files: FileList) -> Option<String> {
        let (paths, cut) = &files;
        self.consumed.clear();
        match onyx_windows::set_clipboard_files(paths, *cut) {
            Ok(()) => {
                self.fallback = None;
                self.observed = Some(files);
                None
            }
            Err(error) => {
                self.observed = onyx_windows::get_clipboard_files();
                self.fallback = Some(files);
                Some(format!("Available in Onyx only · {error}"))
            }
        }
    }
}

pub(crate) enum Prompt {
    NewFolder(PathBuf),
    Rename(PathBuf),
    PermanentDelete(PathBuf),
}

enum Operation {
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
        dest_zip: PathBuf,
    },
    ExtractArchive {
        archive_path: PathBuf,
        dest_dir: PathBuf,
    },
    Recycle(PathBuf),
    PermanentDelete(PermanentDeleteRequest),
    Undo,
}

impl Operation {
    fn title(&self) -> &'static str {
        match self {
            Self::Create { .. } => "New folder",
            Self::Rename { .. } => "Rename",
            Self::Duplicate(_) => "Duplicate",
            Self::Paste(_) => "Paste",
            Self::CopyItems { .. } => "Copy",
            Self::MoveItems { .. } => "Move",
            Self::CreateArchive { .. } => "Compress to ZIP",
            Self::ExtractArchive { .. } => "Extract ZIP",
            Self::Recycle(_) => "Move to Recycle Bin",
            Self::PermanentDelete(_) => "Permanent delete",
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
            Self::CopyItems {
                sources,
                destination,
            } => {
                affected.push(destination.clone());
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
            Self::CreateArchive { sources, dest_zip } => {
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
            Self::Undo => return engine.undo_last().await,
        };
        report.map(Some)
    }
}

fn add_parent(affected: &mut Vec<PathBuf>, path: &Path) {
    if let Some(parent) = path.parent()
        && !affected.iter().any(|existing| existing == parent)
    {
        affected.push(parent.to_path_buf());
    }
}

/// Maps `AppConfig::default_terminal` (`"wt"`, `"pwsh"`, `"cmd"`) to [`TerminalProfile`].
pub(crate) fn terminal_profile_from_config(value: &str) -> TerminalProfile {
    match value.trim().to_ascii_lowercase().as_str() {
        "pwsh" | "powershell" => TerminalProfile::PowerShell,
        "cmd" | "command-prompt" => TerminalProfile::Cmd,
        _ => TerminalProfile::WindowsTerminal,
    }
}

/// Computes a safe target ZIP path inside `current_dir` for `sources`, auto-renaming if needed.
pub(crate) fn compute_archive_dest_path(
    sources: &[PathBuf],
    current_dir: &Path,
) -> OnyxResult<PathBuf> {
    let base_name = if sources.len() == 1 {
        let only = &sources[0];
        let stem = if only.is_dir() {
            only.file_name()
        } else {
            only.file_stem().or_else(|| only.file_name())
        };
        let label = stem
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("Archive");
        format!("{label}.zip")
    } else {
        "Archive.zip".to_string()
    };

    let candidate = current_dir.join(base_name);
    if candidate.exists() {
        generate_auto_rename_path(&candidate)
    } else {
        Ok(candidate)
    }
}

/// Computes `<parent>/<stem>` extraction folder destination for `archive_path`.
pub(crate) fn compute_extract_folder_dest(archive_path: &Path) -> PathBuf {
    let parent = archive_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stem = archive_path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Extracted");
    parent.join(stem)
}

impl Controller {
    pub(crate) fn start_clipboard_monitor(
        &self,
        mut commands: mpsc::UnboundedReceiver<ClipboardCommand>,
    ) {
        let weak = self.ui.clone();
        let shutdown = self.shutdown.clone();
        let runtime = self.runtime.handle().clone();
        self.runtime.spawn_blocking(move || {
            let mut clipboard = Clipboard::default();
            let mut last_has_files = false;
            let mut timer =
                runtime.block_on(async { tokio::time::interval(Duration::from_millis(800)) });
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let command = runtime.block_on(async {
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
                        post(&weak, move |_, ui| ui.set_status_message(message.into()));
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
                    post(&weak, move |_, ui| ui.set_can_paste(has_files));
                }
            }
        });
    }

    pub(crate) fn copy_selection(&self, ui: &OnyxWindow, cut: bool) {
        if let Some(entry) = self.selected() {
            ui.set_can_paste(true);
            let _ = self
                .clipboard
                .send(ClipboardCommand::Write((vec![entry.path], cut)));
        }
    }

    pub(crate) fn prompt_new_folder(&self, ui: &OnyxWindow) {
        let parent = self.state.borrow().path(self.active()).to_path_buf();
        self.prompt(
            ui,
            Prompt::NewFolder(parent),
            "New folder",
            "Enter a name for the folder.",
            "",
            "Create",
        );
    }

    pub(crate) fn prompt_rename(&self, ui: &OnyxWindow) {
        if let Some(entry) = self.selected() {
            self.prompt(
                ui,
                Prompt::Rename(entry.path),
                "Rename",
                "Enter a new name.",
                &entry.name,
                "Rename",
            );
        }
    }

    pub(crate) fn prompt_delete(&self, ui: &OnyxWindow) {
        if let Some(entry) = self.selected() {
            self.prompt(
                ui,
                Prompt::PermanentDelete(entry.path),
                "Permanently delete?",
                &format!(
                    "Permanently delete “{}”? This cannot be undone.",
                    entry.name
                ),
                "",
                "Delete permanently",
            );
        }
    }

    fn prompt(
        &self,
        ui: &OnyxWindow,
        prompt: Prompt,
        title: &str,
        message: &str,
        input: &str,
        confirm: &str,
    ) {
        let mut state = self.state.borrow_mut();
        if state.operation.is_some() {
            ui.set_status_message("A file operation is already in progress.".into());
            return;
        }
        let danger = matches!(prompt, Prompt::PermanentDelete(_));
        state.prompt = Some(prompt);
        ui.set_modal_title(title.into());
        ui.set_modal_message(message.into());
        ui.set_modal_input(input.into());
        ui.set_modal_confirm_label(confirm.into());
        ui.set_modal_danger(danger);
        ui.set_modal_show_input(!danger);
        ui.set_modal_open(true);
    }

    pub(crate) fn cancel_prompt(&self, ui: &OnyxWindow) {
        self.state.borrow_mut().prompt = None;
        ui.set_modal_open(false);
    }

    pub(crate) fn confirm_prompt(&self, ui: &OnyxWindow, input: &str) {
        let prompt = self.state.borrow_mut().prompt.take();
        ui.set_modal_open(false);
        let operation = match prompt {
            Some(Prompt::NewFolder(parent)) => Operation::Create {
                parent,
                name: input.to_owned(),
            },
            Some(Prompt::Rename(source)) => Operation::Rename {
                source,
                name: input.to_owned(),
            },
            Some(Prompt::PermanentDelete(path)) => {
                Operation::PermanentDelete(PermanentDeleteRequest::confirmed(vec![path]))
            }
            None => return,
        };
        self.start_operation(ui, operation);
    }

    pub(crate) fn duplicate(&self, ui: &OnyxWindow) {
        if let Some(entry) = self.selected() {
            self.start_operation(ui, Operation::Duplicate(entry.path));
        }
    }

    pub(crate) fn paste(&self, ui: &OnyxWindow) {
        let destination = self.state.borrow().path(self.active()).to_path_buf();
        self.start_operation(ui, Operation::Paste(destination));
    }

    pub(crate) fn recycle(&self, ui: &OnyxWindow) {
        if let Some(entry) = self.selected() {
            self.start_operation(ui, Operation::Recycle(entry.path));
        }
    }

    pub(crate) fn undo(&self, ui: &OnyxWindow) {
        self.start_operation(ui, Operation::Undo);
    }

    pub(crate) fn cancel_operation(&self, ui: &OnyxWindow) {
        if let Some((cancel, _)) = &self.state.borrow().operation {
            cancel.cancel();
            ui.set_status_message("Cancelling file operation…".into());
        }
    }

    /// Handles files dropped onto the Onyx window from Windows Explorer or another app.
    pub(crate) fn handle_external_drop(&self, ui: &OnyxWindow, event: DropEvent) {
        if event.paths.is_empty() {
            return;
        }
        let destination = self.state.borrow().path(self.active()).to_path_buf();
        self.start_operation(
            ui,
            Operation::CopyItems {
                sources: event.paths,
                destination,
            },
        );
    }

    /// Handles an internal drag-and-drop onto a folder, tab, or opposite pane.
    pub(crate) fn internal_drop(
        &self,
        ui: &OnyxWindow,
        source: PathBuf,
        destination: PathBuf,
        copy: bool,
    ) {
        if source.as_os_str().is_empty() || destination.as_os_str().is_empty() {
            return;
        }
        if ui_sync::same_path(&source, &destination) {
            return;
        }
        if !copy
            && source
                .parent()
                .is_some_and(|parent| ui_sync::same_path(parent, &destination))
        {
            return;
        }
        let operation = if copy {
            Operation::CopyItems {
                sources: vec![source],
                destination,
            }
        } else {
            Operation::MoveItems {
                sources: vec![source],
                destination,
            }
        };
        self.start_operation(ui, operation);
    }

    /// Initiates an outbound Windows OLE file drag (`DoDragDrop`) on a dedicated STA thread.
    pub(crate) fn external_drag(&self, ui: &OnyxWindow) {
        let (paths, source_dir) = {
            let state = self.state.borrow();
            let active = state.active;
            (
                state.selected_paths(active),
                state.path(active).to_path_buf(),
            )
        };
        if paths.is_empty() {
            ui.set_status_message("Select an item before dragging to an external app.".into());
            return;
        }
        ui.set_status_message("External drag active… drop onto another application.".into());
        let weak = self.ui.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("onyx-ole-drag".into())
            .spawn(move || {
                let result = onyx_windows::start_external_file_drag(&paths);
                post(&weak, move |app, ui| match result {
                    Ok(DragOutcome::Copied) => {
                        ui.set_status_message("Dragged item copied to external app.".into());
                    }
                    Ok(DragOutcome::Moved) => {
                        app.refresh_paths(ui, &[source_dir]);
                        ui.set_status_message("Dragged item moved to external app.".into());
                    }
                    Ok(DragOutcome::Cancelled) => {
                        ui.set_status_message("External drag cancelled.".into());
                    }
                    Err(error) => {
                        ui.set_status_message(format!("External drag failed · {error}").into());
                    }
                });
            })
        {
            ui.set_status_message(format!("Could not start external drag: {error}").into());
        }
    }

    /// Compresses the currently selected item(s) into a ZIP archive in the active directory.
    pub(crate) fn compress_zip(&self, ui: &OnyxWindow) {
        let (sources, current_dir) = {
            let state = self.state.borrow();
            let active = state.active;
            (
                state.selected_paths(active),
                state.path(active).to_path_buf(),
            )
        };
        if sources.is_empty() {
            ui.set_status_message("Select one or more items to compress.".into());
            return;
        }
        let dest_zip = match compute_archive_dest_path(&sources, &current_dir) {
            Ok(path) => path,
            Err(error) => {
                ui.set_status_message(error.to_string().into());
                return;
            }
        };
        self.start_operation(ui, Operation::CreateArchive { sources, dest_zip });
    }

    /// Extracts the selected `.zip` archive directly into the active directory.
    pub(crate) fn extract_here(&self, ui: &OnyxWindow) {
        let Some(entry) = self
            .selected()
            .filter(|e| !e.is_dir() && is_zip_path(&e.path))
        else {
            ui.set_status_message("Select a .zip archive to extract.".into());
            return;
        };
        let dest_dir = self.state.borrow().path(self.active()).to_path_buf();
        self.start_operation(
            ui,
            Operation::ExtractArchive {
                archive_path: entry.path,
                dest_dir,
            },
        );
    }

    /// Extracts the selected `.zip` archive into `<dir>/<stem>/`.
    pub(crate) fn extract_to_folder(&self, ui: &OnyxWindow) {
        let Some(entry) = self
            .selected()
            .filter(|e| !e.is_dir() && is_zip_path(&e.path))
        else {
            ui.set_status_message("Select a .zip archive to extract.".into());
            return;
        };
        let dest_dir = compute_extract_folder_dest(&entry.path);
        self.start_operation(
            ui,
            Operation::ExtractArchive {
                archive_path: entry.path,
                dest_dir,
            },
        );
    }

    /// Opens the Archive Browser modal for the currently selected `.zip` file.
    pub(crate) fn browse_archive(&self, ui: &OnyxWindow) {
        let Some(entry) = self
            .selected()
            .filter(|e| !e.is_dir() && is_zip_path(&e.path))
        else {
            ui.set_status_message("Select a .zip archive to browse.".into());
            return;
        };
        self.browse_archive_path(ui, entry.path);
    }

    /// Asynchronously lists entries of `archive_path` and presents them in the Archive Browser overlay.
    pub(crate) fn browse_archive_path(&self, ui: &OnyxWindow, archive_path: PathBuf) {
        let cancel = {
            let mut state = self.state.borrow_mut();
            state.archive_cancel.cancel();
            state.archive_cancel = CancellationToken::new();
            state.archive_path = Some(archive_path.clone());
            state.archive_cancel.clone()
        };

        let title = archive_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Archive.zip".to_string());
        ui.set_archive_title(title.into());
        ui.set_archive_path(ui_sync::display_path(&archive_path));
        ui.set_archive_summary("Reading archive…".into());
        ui.set_archive_error("".into());
        ui.set_archive_entries(ModelRc::default());
        ui.set_archive_loading(true);
        ui.set_archive_open(true);

        let weak = self.ui.clone();
        self.runtime.spawn(async move {
            let result = list_archive_entries(archive_path, cancel.clone()).await;
            post(&weak, move |_, ui| {
                if cancel.is_cancelled() {
                    return;
                }
                ui.set_archive_loading(false);
                match result {
                    Ok(mut entries) => {
                        entries.sort_by(|a, b| {
                            b.is_dir
                                .cmp(&a.is_dir)
                                .then_with(|| a.relative_path.cmp(&b.relative_path))
                        });
                        let total_size: u64 = entries
                            .iter()
                            .filter(|e| !e.is_dir)
                            .map(|e| e.size)
                            .fold(0u64, u64::saturating_add);
                        let summary = format_archive_summary(&entries, total_size);
                        ui.set_archive_summary(summary.into());
                        ui.set_archive_entries(ui_sync::archive_entries(&entries));
                    }
                    Err(error) => {
                        ui.set_archive_summary("".into());
                        ui.set_archive_error(error.to_string().into());
                    }
                }
            });
        });
    }

    pub(crate) fn close_archive_browser(&self, ui: &OnyxWindow) {
        {
            let mut state = self.state.borrow_mut();
            state.archive_cancel.cancel();
            state.archive_path = None;
        }
        ui.set_archive_open(false);
        ui.set_archive_loading(false);
    }

    pub(crate) fn archive_extract_here(&self, ui: &OnyxWindow) {
        let Some(archive_path) = self.state.borrow().archive_path.clone() else {
            return;
        };
        let dest_dir = archive_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or_else(
                || self.state.borrow().path(self.active()).to_path_buf(),
                Path::to_path_buf,
            );
        self.close_archive_browser(ui);
        self.start_operation(
            ui,
            Operation::ExtractArchive {
                archive_path,
                dest_dir,
            },
        );
    }

    pub(crate) fn archive_extract_to_folder(&self, ui: &OnyxWindow) {
        let Some(archive_path) = self.state.borrow().archive_path.clone() else {
            return;
        };
        let dest_dir = compute_extract_folder_dest(&archive_path);
        self.close_archive_browser(ui);
        self.start_operation(
            ui,
            Operation::ExtractArchive {
                archive_path,
                dest_dir,
            },
        );
    }

    fn start_operation(&self, ui: &OnyxWindow, operation: Operation) {
        let mut state = self.state.borrow_mut();
        if state.operation.is_some() {
            ui.set_status_message("A file operation is already in progress.".into());
            return;
        }
        let title = operation.title();
        ui.set_status_message("".into());
        ui.set_operation_progress(UiOperationProgress {
            active: true,
            title: title.into(),
            ..UiOperationProgress::default()
        });
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let engine = state.operations.clone();
        let clipboard = self.clipboard.clone();
        let weak = self.ui.clone();
        let undo = matches!(operation, Operation::Undo);
        let task = self.runtime.spawn(async move {
            let (sender, mut receiver) = mpsc::channel(16);
            let drain = async {
                while let Some(progress) = receiver.recv().await {
                    post(&weak, move |_, ui| show_progress(ui, title, progress));
                }
            };
            let mut affected = Vec::new();
            let (result, ()) = tokio::join!(
                operation.run(
                    engine.clone(),
                    clipboard,
                    worker_cancel,
                    sender,
                    &mut affected
                ),
                drain
            );
            let can_undo = engine.can_undo().await;
            post(&weak, move |app, ui| {
                {
                    let mut state = app.state.borrow_mut();
                    state.operation = None;
                    if undo {
                        state.directories.clear();
                        state.indexed_roots.clear();
                        if let Some((_, cancel)) = state.indexing.take() {
                            cancel.cancel();
                        }
                        affected
                            .extend(state.visible_panes().map(|id| state.path(id).to_path_buf()));
                    }
                }
                ui.set_operation_progress(UiOperationProgress::default());
                ui.set_can_undo(can_undo);
                app.refresh_paths(ui, &affected);
                ui.set_status_message(operation_message(title, result).into());
            });
        });
        state.operation = Some((cancel, task));
    }

    pub(crate) fn properties(&self, _: &OnyxWindow) {
        let path = {
            let state = self.state.borrow();
            state.selected(state.active).map_or_else(
                || state.path(state.active).to_path_buf(),
                |entry| entry.path.clone(),
            )
        };
        self.shell_task(path, onyx_windows::show_properties_dialog);
    }

    pub(crate) fn open_with(&self, _: &OnyxWindow) {
        if let Some(entry) = self.selected().filter(|entry| !entry.is_dir()) {
            self.shell_task(entry.path, onyx_windows::open_with_dialog);
        }
    }

    pub(crate) fn context_menu(&self, ui: &OnyxWindow, pane: PaneId, index: i32) {
        let (clicked, selection, pane_path) = {
            let state = self.state.borrow();
            let Some(entry) = usize::try_from(index)
                .ok()
                .and_then(|index| state.pane(pane).entries.get(index))
            else {
                return;
            };
            let selection = state
                .tab(pane)
                .filter(|tab| tab.selected_paths.contains(&entry.path))
                .map(|tab| tab.selected_paths.clone());
            (
                entry.path.clone(),
                selection,
                state.path(pane).to_path_buf(),
            )
        };
        if selection.is_some() {
            self.activate_pane(ui, pane);
        } else {
            self.select(ui, pane, index);
        }
        let Some(hwnd) = ui_sync::window_hwnd(ui) else {
            ui.set_status_message("The Windows context menu requires a window handle.".into());
            return;
        };
        let parent = clicked.parent();
        let changed_parent = parent
            .filter(|parent| *parent != pane_path)
            .map(Path::to_path_buf);
        let mut affected = vec![pane_path];
        affected.extend(changed_parent);
        let paths = selection
            .filter(|paths| paths.iter().all(|path| path.parent() == parent))
            .unwrap_or_else(|| vec![clicked]);

        let weak = self.ui.clone();
        // The native menu initializes COM as STA and blocks until dismissal.
        if let Err(error) = std::thread::Builder::new()
            .name("onyx-shell-menu".into())
            .spawn(move || {
                let result = onyx_windows::show_context_menu(hwnd, &paths);
                post(&weak, move |app, ui| {
                    app.refresh_paths(ui, &affected);
                    if let Err(error) = result {
                        ui.set_status_message(error.to_string().into());
                    }
                });
            })
        {
            ui.set_status_message(
                format!("Could not start the Windows context menu: {error}").into(),
            );
        }
    }

    pub(crate) fn terminal(&self, _: &OnyxWindow) {
        let (path, profile) = {
            let state = self.state.borrow();
            (
                state.path(state.active).to_path_buf(),
                terminal_profile_from_config(state.config.normalized_terminal()),
            )
        };
        self.shell_task(path, move |path| {
            onyx_windows::launch_terminal_with_profile(path, profile)
        });
    }

    pub(crate) fn terminal_with_profile(&self, profile: TerminalProfile) {
        let path = self.state.borrow().path(self.active()).to_path_buf();
        self.shell_task(path, move |path| {
            onyx_windows::launch_terminal_with_profile(path, profile)
        });
    }

    pub(crate) fn shell_task(
        &self,
        path: PathBuf,
        action: impl FnOnce(&Path) -> OnyxResult<()> + Send + 'static,
    ) {
        let weak = self.ui.clone();
        self.runtime.spawn_blocking(move || {
            if let Err(error) = action(&path) {
                post(&weak, move |_, ui| {
                    ui.set_status_message(error.to_string().into())
                });
            }
        });
    }
}

fn format_archive_summary(entries: &[ArchiveEntry], total_uncompressed: u64) -> String {
    let file_count = entries.iter().filter(|e| !e.is_dir).count();
    let dir_count = entries.iter().filter(|e| e.is_dir).count();
    let total = entries.len();
    format!(
        "{total} entries ({file_count} files, {dir_count} folders) · {} uncompressed",
        format_bytes(total_uncompressed)
    )
}

fn show_progress(ui: &OnyxWindow, title: &str, progress: OperationProgress) {
    let fraction = if progress.total_bytes > 0 {
        progress.bytes_copied as f32 / progress.total_bytes as f32
    } else if progress.total_items > 0 {
        progress.completed_items as f32 / progress.total_items as f32
    } else {
        0.0
    };
    ui.set_operation_progress(UiOperationProgress {
        active: true,
        title: title.into(),
        current_item: progress.current_item.into(),
        progress: fraction.clamp(0.0, 1.0),
        detail: progress
            .error
            .unwrap_or_else(|| {
                format!(
                    "{} of {} items",
                    progress.completed_items, progress.total_items
                )
            })
            .into(),
    });
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
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn terminal_profile_mapping_covers_all_config_values() {
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
            terminal_profile_from_config("fallback"),
            TerminalProfile::WindowsTerminal
        );
    }

    #[test]
    fn archive_destination_and_extract_folder_helpers() {
        let dir = TempDir::new().unwrap();
        let single_file = dir.path().join("report.txt");
        std::fs::write(&single_file, b"hello").unwrap();

        let zip1 =
            compute_archive_dest_path(std::slice::from_ref(&single_file), dir.path()).unwrap();
        assert_eq!(zip1, dir.path().join("report.zip"));

        // Creating `report.zip` triggers auto-rename on next archive creation
        std::fs::write(&zip1, b"existing").unwrap();
        let zip2 =
            compute_archive_dest_path(std::slice::from_ref(&single_file), dir.path()).unwrap();
        assert_eq!(zip2, dir.path().join("report (2).zip"));

        let multi =
            compute_archive_dest_path(&[single_file, dir.path().join("other.txt")], dir.path())
                .unwrap();
        assert_eq!(multi, dir.path().join("Archive.zip"));

        let extract_dir = compute_extract_folder_dest(&dir.path().join("bundle.zip"));
        assert_eq!(extract_dir, dir.path().join("bundle"));
    }
}
