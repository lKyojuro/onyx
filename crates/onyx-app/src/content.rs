use onyx_cache::RgbaImageData;
use onyx_core::{FileEntry, GenerationId, OnyxError, OnyxResult, PaneId, SortSpec, sort_entries};
use onyx_preview::PreviewEngine;
use onyx_search::{SearchIndex, SearchQuery, SearchResult};
use onyx_ui::OnyxWindow;
use slint::Model;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::controller::{Controller, post};
use crate::navigation::worker_error;
use crate::ui_sync;

impl Controller {
    pub(crate) fn update_preview(&self, ui: &OnyxWindow) {
        let (entry, cancel) = {
            let mut state = self.state.borrow_mut();
            let entry = (state.config.show_preview_pane || state.quick_look)
                .then(|| state.selected(state.active).cloned())
                .flatten();
            let path = entry.as_ref().map(|entry| entry.path.clone());
            if path == state.preview_path {
                if path.is_none() && state.quick_look {
                    state.quick_look = false;
                    ui.set_quick_look_open(false);
                }
                return;
            }
            state.preview_path = path;
            state.preview_cancel.cancel();
            state.preview_cancel = CancellationToken::new();
            if entry.is_none() && state.quick_look {
                state.quick_look = false;
                ui.set_quick_look_open(false);
            }
            (entry, state.preview_cancel.clone())
        };
        self.start_preview(ui, entry, cancel);
    }

    fn start_preview(&self, ui: &OnyxWindow, entry: Option<FileEntry>, cancel: CancellationToken) {
        ui_sync::reset_preview(ui, entry.as_ref());
        let Some(entry) = entry else { return };
        let weak = self.ui.clone();
        self.runtime.spawn(async move {
            let result = PreviewEngine::new()
                .load_preview(entry.path, cancel.clone())
                .await;
            post(&weak, move |_, ui| {
                if cancel.is_cancelled() {
                    return;
                }
                match result {
                    Ok(content) => ui_sync::preview(ui, content),
                    Err(error) => {
                        ui.set_preview_error(error.to_string().into());
                        ui.set_preview_kind("error".into());
                    }
                }
            });
        });
    }

    pub(crate) fn refresh_preview(&self, ui: &OnyxWindow) {
        let (entry, cancel) = {
            let mut state = self.state.borrow_mut();
            if state.preview_path.is_none() {
                return;
            }
            state.preview_cancel.cancel();
            state.preview_cancel = CancellationToken::new();
            (
                state.selected(state.active).cloned(),
                state.preview_cancel.clone(),
            )
        };
        self.start_preview(ui, entry, cancel);
    }

    pub(crate) fn queue_icons(&self, id: PaneId, first: usize) {
        let work = {
            let mut state = self.state.borrow_mut();
            let mut candidates = HashMap::new();
            for entry in state.pane(id).entries.iter().skip(first) {
                let key = ui_sync::icon_key(entry);
                if state.icons.get(&key).is_none()
                    && !state.pending_icons.contains(&key)
                    && !state.missing_icons.contains(&key)
                {
                    candidates
                        .entry(key)
                        .or_insert_with(|| (entry.path.clone(), entry.is_dir()));
                }
            }
            state.pending_icons.extend(candidates.keys().cloned());
            candidates
        };
        if work.is_empty() {
            return;
        }
        let weak = self.ui.clone();
        let shutdown = self.shutdown.clone();
        self.runtime.spawn_blocking(move || {
            let mut batch = Vec::with_capacity(24);
            for (key, (path, is_dir)) in work {
                if shutdown.is_cancelled() {
                    return;
                }
                let image = onyx_windows::extract_file_icon_rgba(&path, is_dir)
                    .map(|icon| RgbaImageData::new(icon.width, icon.height, icon.pixels));
                batch.push((key, image));
                if batch.len() == 24 {
                    let filled = std::mem::replace(&mut batch, Vec::with_capacity(24));
                    post(&weak, move |app, _| app.icons_loaded(filled));
                }
            }
            if !batch.is_empty() {
                post(&weak, move |app, _| app.icons_loaded(batch));
            }
        });
    }

    fn icons_loaded(&self, icons: Vec<(String, Option<RgbaImageData>)>) {
        let mut state = self.state.borrow_mut();
        let mut loaded = HashMap::new();
        for (key, data) in icons {
            state.pending_icons.remove(&key);
            if let Some(data) = data {
                loaded.insert(key.clone(), ui_sync::rgba_image(&data));
                state.icons.put(key, data);
            } else if state.missing_icons.len() < state.icons.capacity() {
                state.missing_icons.insert(key);
            }
        }
        for pane in &state.panes {
            for (index, entry) in pane.entries.iter().enumerate() {
                if let Some(image) = loaded.get(&ui_sync::icon_key(entry))
                    && let Some(mut row) = pane.model.row_data(index)
                {
                    row.icon = image.clone();
                    row.has_icon = true;
                    pane.model.set_row_data(index, row);
                }
            }
        }
    }

    pub(crate) fn search_changed(&self, ui: &OnyxWindow, query: &str, debounce: bool) {
        let id = self.active();
        let query = query.trim();
        {
            let mut state = self.state.borrow_mut();
            if query == state.pane(id).query && debounce {
                return;
            }
            state.pane_mut(id).query = query.to_owned();
            if let Some(tab) = state.tab_mut(id) {
                tab.selected_paths.clear();
            }
        }
        if query.is_empty() {
            self.load_directory(ui, id, true);
        } else {
            self.run_search(ui, id, debounce);
        }
    }

    pub(crate) fn run_search(&self, ui: &OnyxWindow, id: PaneId, debounce: bool) {
        let (path, engine, query, hide_system, generation, cancel) = {
            let mut state = self.state.borrow_mut();
            let path = state.path(id).to_path_buf();
            let engine = state.search.clone();
            let query = SearchQuery {
                text: state.pane(id).query.clone(),
                include_hidden: state.config.show_hidden,
                use_index: state.indexed_search && state.indexed_roots.contains(&path),
                ..SearchQuery::default()
            };
            let hide_system = !state.config.show_system;
            let pane = state.pane_mut(id);
            let (generation, cancel) = pane.restart();
            pane.entries.clear();
            pane.model.set_vec(Vec::new());
            pane.result_order = true;
            (path, engine, query, hide_system, generation, cancel)
        };
        self.sync_pane(ui, id);
        self.update_preview(ui);
        let weak = self.ui.clone();
        self.runtime.spawn(async move {
            if debounce {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_millis(180)) => {}
                }
            }
            let (sender, mut receiver) = mpsc::channel::<Vec<SearchResult>>(4);
            let drain = async {
                while let Some(batch) = receiver.recv().await {
                    let entries = batch
                        .into_iter()
                        .map(|result| result.entry)
                        .filter(|entry| !hide_system || !entry.attributes.system)
                        .collect();
                    post(&weak, move |app, ui| {
                        app.search_batch(ui, id, generation, entries)
                    });
                }
            };
            let (result, ()) =
                tokio::join!(engine.search_streaming(path, query, cancel, sender), drain);
            post(&weak, move |app, ui| {
                app.search_done(ui, id, generation, result)
            });
        });
    }

    fn search_batch(
        &self,
        ui: &OnyxWindow,
        id: PaneId,
        generation: GenerationId,
        entries: Vec<FileEntry>,
    ) {
        let first = {
            let mut state = self.state.borrow_mut();
            let sort = state.tab(id).map_or_else(SortSpec::default, |tab| tab.sort);
            let pane = state.pane_mut(id);
            if pane.generation != generation || pane.cancel.is_cancelled() {
                return;
            }
            let first = pane.entries.len();
            pane.entries.extend(entries);
            if pane.result_order {
                let pane = state.pane(id);
                pane.model
                    .extend(ui_sync::file_rows(&state, id, &pane.entries[first..]));
                first
            } else {
                sort_entries(&mut pane.entries, sort);
                ui_sync::rows(&state, id);
                0
            }
        };
        self.sync_pane(ui, id);
        self.update_preview(ui);
        self.queue_icons(id, first);
    }

    fn search_done(
        &self,
        ui: &OnyxWindow,
        id: PaneId,
        generation: GenerationId,
        result: OnyxResult<usize>,
    ) {
        {
            let mut state = self.state.borrow_mut();
            let pane = state.pane_mut(id);
            if pane.generation != generation || pane.cancel.is_cancelled() {
                return;
            }
            pane.loading = false;
            if let Err(error) = result {
                pane.error = error.to_string();
            }
            if ui_sync::selected_index(&state, id).is_none()
                && let Some(tab) = state.tab_mut(id)
            {
                tab.selected_paths.clear();
            }
        }
        self.sync_pane(ui, id);
        self.update_preview(ui);
    }

    pub(crate) fn toggle_index(&self, ui: &OnyxWindow) {
        let enabled = {
            let mut state = self.state.borrow_mut();
            state.indexed_search = !state.indexed_search;
            if !state.indexed_search
                && let Some((_, cancel)) = state.indexing.take()
            {
                cancel.cancel();
                ui.set_status_message("Indexed search disabled.".into());
            }
            state.indexed_search
        };
        if enabled {
            self.index_current(ui);
        } else {
            self.sync_chrome(ui);
            self.restart_searches(ui);
        }
    }

    fn restart_searches(&self, ui: &OnyxWindow) {
        let panes: Vec<_> = {
            let state = self.state.borrow();
            state
                .visible_panes()
                .filter(|&id| !state.pane(id).query.is_empty())
                .collect()
        };
        for id in panes {
            self.run_search(ui, id, false);
        }
    }

    pub(crate) fn index_current(&self, ui: &OnyxWindow) {
        let (root, db, exclusions, cancel) = {
            let mut state = self.state.borrow_mut();
            state.indexed_search = true;
            let root = state.path(state.active).to_path_buf();
            if let Some((_, previous)) = state.indexing.take() {
                previous.cancel();
            }
            state.indexed_roots.remove(&root);
            let cancel = CancellationToken::new();
            state.indexing = Some((root.clone(), cancel.clone()));
            let mut exclusions = state.config.excluded_index_dirs.clone();
            if let Some(cache_dir) = state.index_path.parent() {
                exclusions.push(cache_dir.to_path_buf());
            }
            (root, state.index_path.clone(), exclusions, cancel)
        };
        self.sync_chrome(ui);
        self.restart_searches(ui);
        ui.set_status_message(format!("Indexing {}…", root.display()).into());

        let weak = self.ui.clone();
        self.runtime.spawn(async move {
            let indexed = tokio::task::spawn_blocking({
                let root = root.clone();
                let cancel = cancel.clone();
                move || -> OnyxResult<_> {
                    if cancel.is_cancelled() {
                        return Err(OnyxError::Cancelled);
                    }
                    let index = SearchIndex::open(&db)?.with_excluded_dirs(exclusions);
                    let count = index.index_directory(&root, &cancel)?;
                    if cancel.is_cancelled() {
                        return Err(OnyxError::Cancelled);
                    }
                    index.reconcile_directory(&root)?;
                    Ok((count, index))
                }
            })
            .await
            .map_err(worker_error)
            .and_then(|result| result);
            post(&weak, move |app, ui| {
                if cancel.is_cancelled() {
                    return;
                }
                app.state.borrow_mut().indexing = None;
                match indexed {
                    Ok((count, index)) => {
                        let mut state = app.state.borrow_mut();
                        state.search = onyx_search::UnifiedSearchEngine::with_index(
                            std::sync::Arc::new(std::sync::Mutex::new(index)),
                        );
                        state.indexed_roots.insert(root);
                        drop(state);
                        ui.set_status_message(format!("Indexed {count} items.").into());
                        app.restart_searches(ui);
                    }
                    Err(error) => {
                        ui.set_status_message(format!("Using live search · {error}").into());
                    }
                }
            });
        });
    }
}
