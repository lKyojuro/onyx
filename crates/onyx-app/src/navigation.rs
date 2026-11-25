use onyx_core::{
    FileEntry, GenerationId, OnyxError, OnyxResult, PaneId, SortColumn, SortDirection, SortSpec,
    sort_entries,
};
use onyx_fs::{EnumerationOptions, FsEngine};
use onyx_ui::OnyxWindow;
use std::path::{Path, PathBuf};

use crate::controller::{Controller, post};
use crate::state::is_zip_path;
use crate::ui_sync;

impl Controller {
    pub(crate) fn navigate_to(&self, ui: &OnyxWindow, id: PaneId, path: PathBuf) {
        self.activate_pane(ui, id);
        if self.active() != id {
            return;
        }
        self.change_path(ui, id, path, true);
    }

    fn change_path(&self, ui: &OnyxWindow, id: PaneId, path: PathBuf, record: bool) {
        let watch_path = path.clone();
        {
            let mut state = self.state.borrow_mut();
            if let Some(tab) = state.tab_mut(id) {
                tab.navigate(path.clone(), record);
            }
            state.config.record_recent(path);
            state.pane_mut(id).query.clear();
        }
        ui.set_address_editing(false);
        self.sync_chrome(ui);
        self.update_watcher();
        self.watch_search_root(watch_path);
        self.load_directory(ui, id, true);
    }

    pub(crate) fn submit_address(&self, ui: &OnyxWindow, value: &str) {
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or(value);
        let id = self.active();
        if value.is_empty() {
            ui.set_address_editing(false);
            self.sync_chrome(ui);
            return;
        }
        let base = self.state.borrow().path(id).to_path_buf();
        let joined = base.join(value);
        let path = std::path::absolute(&joined).unwrap_or(joined);
        self.change_path(ui, id, path, true);
    }

    pub(crate) fn history(&self, ui: &OnyxWindow, forward: bool) {
        let id = self.active();
        let path = self.state.borrow_mut().tab_mut(id).and_then(|tab| {
            if forward {
                tab.history.go_forward()
            } else {
                tab.history.go_back()
            }
            .cloned()
        });
        if let Some(path) = path {
            self.change_path(ui, id, path, false);
        }
    }

    pub(crate) fn go_up(&self, ui: &OnyxWindow) {
        let id = self.active();
        let parent = self.state.borrow().path(id).parent().map(Path::to_path_buf);
        if let Some(path) = parent
            && path.has_root()
        {
            self.change_path(ui, id, path, true);
        }
    }

    pub(crate) fn new_tab(&self, ui: &OnyxWindow) {
        {
            let mut state = self.state.borrow_mut();
            let path = state.path(state.active).to_path_buf();
            state.tabs.new_tab(path);
        }
        self.primary_tab_changed(ui);
    }

    pub(crate) fn close_tab(&self, ui: &OnyxWindow, id: i32) {
        let changed = {
            let mut state = self.state.borrow_mut();
            let index = if id == -1 {
                Some(state.tabs.active_index())
            } else {
                tab_index(&state, id)
            };
            let active_id = state.tabs.active_tab().map(|tab| tab.id);
            if !index.is_some_and(|index| state.tabs.close_tab(index)) {
                return;
            }
            active_id != state.tabs.active_tab().map(|tab| tab.id)
        };
        if changed {
            self.primary_tab_changed(ui);
        } else {
            self.sync_chrome(ui);
        }
    }

    pub(crate) fn switch_tab(&self, ui: &OnyxWindow, id: i32) {
        let switched = {
            let mut state = self.state.borrow_mut();
            tab_index(&state, id).is_some_and(|index| state.tabs.switch_tab(index))
        };
        if switched {
            self.primary_tab_changed(ui);
        }
    }

    pub(crate) fn reorder_tab(&self, ui: &OnyxWindow, from: i32, to: i32) {
        let changed = if let (Ok(from), Ok(to)) = (usize::try_from(from), usize::try_from(to)) {
            self.state.borrow_mut().tabs.reorder_tab(from, to)
        } else {
            false
        };
        if changed {
            self.sync_chrome(ui);
        }
    }

    fn primary_tab_changed(&self, ui: &OnyxWindow) {
        {
            let mut state = self.state.borrow_mut();
            state.active = PaneId::Primary;
            state.pane_mut(PaneId::Primary).query.clear();
        }
        ui.set_address_editing(false);
        self.sync_chrome(ui);
        self.update_watcher();
        self.load_directory(ui, PaneId::Primary, true);
    }

    pub(crate) fn refresh(&self, ui: &OnyxWindow) {
        let path = self.state.borrow().path(self.active()).to_path_buf();
        self.refresh_paths(ui, &[path]);
    }

    pub(crate) fn refresh_paths(&self, ui: &OnyxWindow, paths: &[PathBuf]) {
        let panes: Vec<_> = {
            let mut state = self.state.borrow_mut();
            for path in paths {
                state.directories.invalidate(path);
            }
            let panes: Vec<_> = state
                .visible_panes()
                .filter(|&id| paths.iter().any(|path| paths_overlap(path, state.path(id))))
                .collect();
            for &id in &panes {
                state.directories.invalidate(state.path(id));
            }
            state
                .indexed_roots
                .retain(|root| !paths.iter().any(|path| paths_overlap(root, path)));
            if state
                .indexing
                .as_ref()
                .is_some_and(|(root, _)| paths.iter().any(|path| paths_overlap(root, path)))
                && let Some((_, cancel)) = state.indexing.take()
            {
                cancel.cancel();
                ui.set_status_message("Indexing cancelled because the folder changed.".into());
            }
            panes
        };
        for pane in panes {
            self.reload(ui, pane, false);
        }
        let preview_path = self.state.borrow().preview_path.clone();
        if let Some(path) = preview_path
            && path.parent().is_some_and(|parent| {
                paths
                    .iter()
                    .any(|changed| ui_sync::same_path(changed, parent))
            })
        {
            // The selection can remain the same while its contents change.
            self.refresh_preview(ui);
        }
    }

    pub(crate) fn reload(&self, ui: &OnyxWindow, id: PaneId, clear_rows: bool) {
        if self.state.borrow().pane(id).query.is_empty() {
            self.load_directory(ui, id, clear_rows);
        } else {
            self.run_search(ui, id, false);
        }
    }

    pub(crate) fn load_directory(&self, ui: &OnyxWindow, id: PaneId, clear_rows: bool) {
        let (path, options, generation, cancel, has_cached_rows) = {
            let mut state = self.state.borrow_mut();
            let path = state.path(id).to_path_buf();
            let sort = state.tab(id).map_or_else(SortSpec::default, |tab| tab.sort);
            let options = EnumerationOptions {
                show_hidden: state.config.show_hidden,
                show_system: state.config.show_system,
                sort,
                ..EnumerationOptions::default()
            };
            let cached = if clear_rows {
                state.directories.get(&path)
            } else {
                None
            };
            let has_cached = cached.as_ref().is_some_and(|entries| !entries.is_empty());
            let pane = state.pane_mut(id);
            let (generation, cancel) = pane.restart();
            if clear_rows {
                pane.entries = cached.unwrap_or_default();
                sort_entries(&mut pane.entries, sort);
                ui_sync::rows(&state, id);
            }
            (path, options, generation, cancel, has_cached)
        };
        self.sync_pane(ui, id);
        self.update_preview(ui);
        self.queue_icons(id, 0);
        if has_cached_rows {
            self.queue_thumbnails(id);
        }
        let weak = self.ui.clone();
        self.runtime.spawn(async move {
            let result = FsEngine::new()
                .enumerate_dir(path.clone(), generation, options, cancel)
                .await;
            post(&weak, move |app, ui| {
                app.directory_loaded(ui, id, generation, path, result)
            });
        });
    }

    fn directory_loaded(
        &self,
        ui: &OnyxWindow,
        id: PaneId,
        generation: GenerationId,
        path: PathBuf,
        result: OnyxResult<Vec<FileEntry>>,
    ) {
        {
            let mut state = self.state.borrow_mut();
            if state.pane(id).generation != generation || state.pane(id).cancel.is_cancelled() {
                return;
            }
            state.pane_mut(id).loading = false;
            match result {
                Ok(mut entries) => {
                    let sort = state.tab(id).map_or_else(SortSpec::default, |tab| tab.sort);
                    sort_entries(&mut entries, sort);
                    state.directories.put(path, entries.clone());
                    state.pane_mut(id).entries = entries;
                }
                Err(error) => {
                    state.pane_mut(id).entries.clear();
                    state.pane_mut(id).error = error.to_string();
                }
            }
            if ui_sync::selected_index(&state, id).is_none()
                && let Some(tab) = state.tab_mut(id)
            {
                tab.selected_paths.clear();
            }
            ui_sync::rows(&state, id);
        }
        self.sync_pane(ui, id);
        self.update_preview(ui);
        self.queue_icons(id, 0);
        self.queue_thumbnails(id);
    }

    pub(crate) fn sort_by(&self, ui: &OnyxWindow, id: PaneId, column: i32) {
        let column = match column {
            0 => SortColumn::Name,
            1 => SortColumn::Modified,
            2 => SortColumn::Kind,
            3 => SortColumn::Size,
            _ => return,
        };
        {
            let mut state = self.state.borrow_mut();
            let Some(tab) = state.tab_mut(id) else { return };
            tab.sort.direction = if tab.sort.column == column {
                tab.sort.direction.toggled()
            } else {
                SortDirection::Ascending
            };
            tab.sort.column = column;
            let sort = tab.sort;
            let pane = state.pane_mut(id);
            pane.result_order = false;
            sort_entries(&mut pane.entries, sort);
            ui_sync::rows(&state, id);
        }
        self.sync_pane(ui, id);
        self.queue_thumbnails(id);
    }

    pub(crate) fn select(&self, ui: &OnyxWindow, id: PaneId, index: i32) {
        if id == PaneId::Secondary && !self.state.borrow().dual {
            return;
        }
        {
            let mut state = self.state.borrow_mut();
            let previous = ui_sync::selected_index(&state, id);
            let index = usize::try_from(index).ok();
            let path = index
                .and_then(|i| state.pane(id).entries.get(i))
                .map(|entry| entry.path.clone());
            if let Some(tab) = state.tab_mut(id) {
                tab.selected_paths = path.into_iter().collect();
            }
            if let Some(previous) = previous {
                ui_sync::selection_row(&state, id, previous, false);
            }
            if let Some(index) = index {
                ui_sync::selection_row(&state, id, index, true);
            }
        }
        self.activate_pane(ui, id);
        self.sync_pane(ui, id);
        self.update_preview(ui);
    }

    pub(crate) fn move_selection(&self, ui: &OnyxWindow, offset: i32) {
        let id = self.active();
        let index = {
            let state = self.state.borrow();
            let count = state.pane(id).entries.len();
            if count == 0 {
                return;
            }
            ui_sync::selected_index(&state, id)
                .map_or(if offset < 0 { count - 1 } else { 0 }, |index| {
                    index.saturating_add_signed(offset as isize).min(count - 1)
                })
        };
        if let Ok(index) = i32::try_from(index) {
            self.select(ui, id, index);
        }
    }

    pub(crate) fn activate(&self, ui: &OnyxWindow, id: PaneId, index: i32) {
        self.activate_pane(ui, id);
        if self.active() != id {
            return;
        }
        let entry = usize::try_from(index)
            .ok()
            .and_then(|i| self.state.borrow().pane(id).entries.get(i).cloned());
        let Some(entry) = entry else { return };
        if self.state.borrow().quick_look {
            self.toggle_quick_look(ui);
        }
        if entry.is_dir() {
            self.change_path(ui, id, entry.path, true);
        } else if !entry.is_symlink && !entry.attributes.reparse_point && is_zip_path(&entry.path) {
            self.browse_archive_path(ui, entry.path);
        } else if entry.is_symlink || entry.attributes.reparse_point {
            let weak = self.ui.clone();
            let generation = self.state.borrow().pane(id).generation;
            self.runtime.spawn_blocking(move || {
                let directory = crate::is_directory(&entry.path);
                if directory {
                    post(&weak, move |app, ui| {
                        if app.active() == id
                            && app.state.borrow().pane(id).generation == generation
                        {
                            app.change_path(ui, id, entry.path, true);
                        }
                    });
                } else if let Err(error) = onyx_windows::open_file(&entry.path) {
                    post(&weak, move |_, ui| {
                        ui.set_status_message(error.to_string().into())
                    });
                }
            });
        } else {
            self.shell_task(entry.path, onyx_windows::open_file);
        }
    }

    pub(crate) fn toggle_hidden(&self, ui: &OnyxWindow) {
        let enabled = !self.state.borrow().config.show_hidden;
        self.settings_set_show_hidden(ui, enabled);
    }

    pub(crate) fn toggle_favorite(&self, ui: &OnyxWindow) {
        {
            let mut state = self.state.borrow_mut();
            let path = state.path(state.active).to_path_buf();
            if !state.config.remove_favorite(&path) {
                state.config.add_favorite(onyx_core::FavoriteItem {
                    name: onyx_core::navigation::derive_tab_title(&path),
                    path,
                });
            }
            ui_sync::chrome(ui, &state);
        }
        self.persist_config();
    }

    pub(crate) fn unpin_favorite(&self, ui: &OnyxWindow, path: &Path) {
        self.state.borrow_mut().config.remove_favorite(path);
        self.sync_chrome(ui);
        self.persist_config();
    }
}

fn tab_index(state: &crate::state::State, id: i32) -> Option<usize> {
    state.tabs.tabs().iter().position(|tab| tab.id as i32 == id)
}

fn paths_overlap(first: &Path, second: &Path) -> bool {
    let first = onyx_core::normalize_display_path(first)
        .replace('/', "\\")
        .to_lowercase();
    let second = onyx_core::normalize_display_path(second)
        .replace('/', "\\")
        .to_lowercase();
    let first = Path::new(&first);
    let second = Path::new(&second);
    first.starts_with(second) || second.starts_with(first)
}

pub(crate) fn worker_error(error: tokio::task::JoinError) -> OnyxError {
    OnyxError::OperationFailed {
        message: error.to_string(),
    }
}
