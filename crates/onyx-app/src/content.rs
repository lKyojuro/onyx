use onyx_cache::RgbaImageData;
use onyx_core::{
    FileEntry, GenerationId, OnyxError, OnyxResult, PaneId, SortSpec, ViewMode,
    normalize_display_path, sort_entries,
};
use onyx_preview::{PreviewEngine, ThumbnailImage, is_thumbnail_supported};
use onyx_search::{
    IncrementalIndexer, SearchIndex, SearchQuery, SearchResult, UnifiedSearchEngine,
};
use onyx_ui::OnyxWindow;
use slint::Model;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::controller::{Controller, post};
use crate::navigation::worker_error;
use crate::state::{MAX_WATCHED_INDEX_ROOTS, State};
use crate::ui_sync;

/// Target square pixel dimension for Grid View async image thumbnails.
pub(crate) const THUMBNAIL_TARGET_PX: u32 = 128;

/// Maximum concurrent thumbnail worker tasks spawned per pane.
const MAX_THUMBNAIL_WORKERS: usize = 4;

#[derive(Clone)]
pub(crate) struct IndexWatchRequest {
    generation: GenerationId,
    exclusions: Vec<PathBuf>,
    roots: Vec<PathBuf>,
    reconcile_roots: Vec<PathBuf>,
}

impl IndexWatchRequest {
    pub(crate) fn from_state(state: &State) -> Self {
        Self {
            generation: state.indexer_generation,
            exclusions: state.index_exclusions(),
            roots: state.watched_index_roots.iter().cloned().collect(),
            reconcile_roots: Vec::new(),
        }
    }
}

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

    /// Asynchronously loads RGBA8 image thumbnails for supported image files when Grid View is active.
    ///
    /// Tags every decode task with the pane's current [`GenerationId`] and a child [`CancellationToken`]
    /// so navigating away or switching views cancels pending work and drops stale callbacks cleanly.
    pub(crate) fn queue_thumbnails(&self, id: PaneId) {
        let (generation, cancel, engine, pending) = {
            let mut state = self.state.borrow_mut();
            if state.config.default_view != ViewMode::Grid {
                state.pane_mut(id).thumb_cancel.cancel();
                return;
            }
            let engine = state.thumbnails.clone();
            let pane = state.pane_mut(id);
            let (generation, cancel) = pane.restart_thumbnails();
            let mut pending = VecDeque::new();
            for (index, entry) in pane.entries.iter().enumerate() {
                if entry.is_dir() || !is_thumbnail_supported(&entry.path) {
                    continue;
                }
                if pane
                    .model
                    .row_data(index)
                    .is_some_and(|row| row.has_thumbnail)
                {
                    continue;
                }
                pending.push_back(entry.path.clone());
            }
            (generation, cancel, engine, pending)
        };

        if pending.is_empty() {
            return;
        }

        let worker_count = pending.len().min(MAX_THUMBNAIL_WORKERS);
        let shared_queue = Arc::new(Mutex::new(pending));

        for _ in 0..worker_count {
            let queue = Arc::clone(&shared_queue);
            let engine = engine.clone();
            let cancel = cancel.clone();
            let shutdown = self.shutdown.clone();
            let weak = self.ui.clone();
            self.runtime.spawn(async move {
                loop {
                    if cancel.is_cancelled() || shutdown.is_cancelled() {
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
                        .load_thumbnail(path.clone(), THUMBNAIL_TARGET_PX, cancel.clone())
                        .await
                    {
                        Ok(thumb) => {
                            if cancel.is_cancelled() || shutdown.is_cancelled() {
                                break;
                            }
                            post(&weak, move |app, _| {
                                app.thumbnail_loaded(id, generation, path, thumb)
                            });
                        }
                        Err(OnyxError::Cancelled) => break,
                        Err(error) => {
                            tracing::debug!(path = %path.display(), %error, "thumbnail decode skipped");
                        }
                    }
                }
            });
        }
    }

    fn thumbnail_loaded(
        &self,
        id: PaneId,
        generation: GenerationId,
        path: PathBuf,
        thumb: ThumbnailImage,
    ) {
        let state = self.state.borrow();
        if state.config.default_view != ViewMode::Grid {
            return;
        }
        let pane = state.pane(id);
        if pane.generation != generation
            || pane.cancel.is_cancelled()
            || pane.thumb_cancel.is_cancelled()
        {
            return;
        }
        let Some(index) = pane.entries.iter().position(|entry| entry.path == path) else {
            return;
        };
        if let Some(mut row) = pane.model.row_data(index) {
            row.thumbnail = ui_sync::thumbnail_image(&thumb);
            row.has_thumbnail = true;
            pane.model.set_row_data(index, row);
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
        self.queue_thumbnails(id);
    }

    pub(crate) fn start_incremental_indexer(
        &self,
        db_path: PathBuf,
        mut requests: watch::Receiver<IndexWatchRequest>,
    ) -> tokio::task::JoinHandle<()> {
        let weak = self.ui.clone();
        let shutdown = self.shutdown.clone();
        let handle = self.runtime.handle().clone();
        self.watch_initial_search_roots();
        self.runtime.spawn_blocking(move || {
            let mut generation = None;
            let mut indexer: Option<IncrementalIndexer> = None;
            let mut watching = HashSet::<PathBuf>::new();

            while !shutdown.is_cancelled() {
                let request = requests.borrow_and_update().clone();
                let restarting = generation != Some(request.generation);
                if restarting {
                    // The blocking worker owns the watcher, including its synchronous teardown.
                    drop(indexer.take());
                    watching.clear();
                    generation = Some(request.generation);
                    let opened = SearchIndex::open(&db_path).map(|index| {
                        Arc::new(Mutex::new(
                            index.with_excluded_dirs(request.exclusions.clone()),
                        ))
                    });
                    match opened {
                        Ok(index) => {
                            match IncrementalIndexer::start(Arc::clone(&index), request.exclusions)
                            {
                                Ok(started) => indexer = Some(started),
                                Err(error) => {
                                    tracing::warn!(%error, "could not start search index watcher")
                                }
                            }
                            post(&weak, move |app, ui| {
                                let mut state = app.state.borrow_mut();
                                if request.generation != state.indexer_generation {
                                    return;
                                }
                                state.search_index = Some(Arc::clone(&index));
                                state.search = UnifiedSearchEngine::with_index(index);
                                drop(state);
                                app.restart_searches(ui);
                            });
                        }
                        Err(error) => tracing::warn!(%error, "could not open search index"),
                    }
                }

                if let Some(indexer) = &indexer {
                    watching.retain(|path| {
                        if request.roots.contains(path) {
                            true
                        } else {
                            indexer.unwatch_root(path);
                            false
                        }
                    });
                    for root in request.roots {
                        if !watching.contains(&root) {
                            match indexer.watch_root(root.clone()) {
                                Ok(()) => {
                                    watching.insert(root);
                                }
                                Err(error) => {
                                    tracing::debug!(%error, "could not watch search root")
                                }
                            }
                        }
                    }
                    if restarting {
                        for root in request.reconcile_roots {
                            indexer.trigger_reconcile(root);
                        }
                    }
                }
                if !handle.block_on(async {
                    tokio::select! {
                        _ = shutdown.cancelled() => false,
                        changed = requests.changed() => changed.is_ok(),
                    }
                }) {
                    break;
                }
            }
        })
    }

    fn watch_initial_search_roots(&self) {
        let roots: Vec<PathBuf> = {
            let state = self.state.borrow();
            let mut roots = vec![state.home.clone()];
            for pane in state.visible_panes() {
                roots.push(state.path(pane).to_path_buf());
            }
            roots
        };
        for root in roots {
            self.watch_search_root(root);
        }
    }

    /// Schedules a recursive watch without blocking the event loop.
    pub(crate) fn watch_search_root(&self, root: PathBuf) {
        let mut state = self.state.borrow_mut();
        if root.as_os_str().is_empty()
            || is_root_excluded(&root, &state.index_exclusions())
            || state
                .watched_index_roots
                .iter()
                .any(|existing| is_same_or_ancestor(existing, &root))
        {
            return;
        }
        if state.watched_index_roots.len() >= MAX_WATCHED_INDEX_ROOTS {
            let evict_idx = state
                .watched_index_roots
                .iter()
                .position(|path| !ui_sync::same_path(path, &state.home))
                .unwrap_or(0);
            state.watched_index_roots.remove(evict_idx);
        }
        state.watched_index_roots.push_back(root);
        self.index_requests.send_modify(|request| {
            request.roots = state.watched_index_roots.iter().cloned().collect();
        });
    }

    pub(crate) fn restart_incremental_indexer(&self, ui: &OnyxWindow) {
        let mut state = self.state.borrow_mut();
        let exclusions = state.index_exclusions();
        let reconcile_roots = state.indexed_roots.iter().cloned().collect();
        state
            .watched_index_roots
            .retain(|root| !is_root_excluded(root, &exclusions));
        state
            .indexed_roots
            .retain(|root| !is_root_excluded(root, &exclusions));
        if let Some((_, cancel)) = state.indexing.take() {
            cancel.cancel();
        }
        state.indexer_generation = state.indexer_generation.next();
        state.search_index = None;
        state.search = UnifiedSearchEngine::live_only();
        self.index_requests.send_replace(IndexWatchRequest {
            reconcile_roots,
            ..IndexWatchRequest::from_state(&state)
        });
        drop(state);
        self.watch_initial_search_roots();
        self.restart_searches(ui);
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
        let (root, db, exclusions, existing_index, cancel) = {
            let mut state = self.state.borrow_mut();
            state.indexed_search = true;
            let root = state.path(state.active).to_path_buf();
            if let Some((_, previous)) = state.indexing.take() {
                previous.cancel();
            }
            state.indexed_roots.remove(&root);
            let cancel = CancellationToken::new();
            state.indexing = Some((root.clone(), cancel.clone()));
            let exclusions = state.index_exclusions();
            (
                root,
                state.index_path.clone(),
                exclusions,
                state.search_index.clone(),
                cancel,
            )
        };
        self.sync_chrome(ui);
        self.restart_searches(ui);
        ui.set_status_message(format!("Indexing {}…", root.display()).into());

        let weak = self.ui.clone();
        self.runtime.spawn(async move {
            let indexed = tokio::task::spawn_blocking({
                let root = root.clone();
                let cancel = cancel.clone();
                move || -> OnyxResult<(usize, Arc<Mutex<SearchIndex>>)> {
                    if cancel.is_cancelled() {
                        return Err(OnyxError::Cancelled);
                    }
                    let shared = match existing_index {
                        Some(shared) => shared,
                        None => {
                            let idx = SearchIndex::open(&db)?.with_excluded_dirs(exclusions);
                            Arc::new(Mutex::new(idx))
                        }
                    };
                    let count = {
                        let guard = shared
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let count = guard.index_directory(&root, &cancel)?;
                        if cancel.is_cancelled() {
                            return Err(OnyxError::Cancelled);
                        }
                        guard.reconcile_directory_with_cancel(&root, &cancel)?;
                        count
                    };
                    Ok((count, shared))
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
                    Ok((count, shared)) => {
                        let mut state = app.state.borrow_mut();
                        if state.search_index.is_none() {
                            state.search_index = Some(Arc::clone(&shared));
                            state.search = UnifiedSearchEngine::with_index(shared);
                        }
                        state.indexed_roots.insert(root.clone());
                        drop(state);
                        app.watch_search_root(root);
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

pub(crate) fn is_same_or_ancestor(ancestor: &Path, candidate: &Path) -> bool {
    let anc = normalize_display_path(ancestor)
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase();
    let cand = normalize_display_path(candidate)
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase();
    if anc == cand {
        return true;
    }
    Path::new(&cand).starts_with(Path::new(&anc))
}

fn is_root_excluded(root: &Path, exclusions: &[PathBuf]) -> bool {
    for excluded in exclusions {
        if excluded.is_absolute() {
            if is_same_or_ancestor(excluded, root) {
                return true;
            }
        } else {
            let name = excluded.to_string_lossy();
            if root.components().any(|comp| match comp {
                std::path::Component::Normal(part) => {
                    part.to_string_lossy().eq_ignore_ascii_case(&name)
                }
                _ => false,
            }) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestor_and_exclusion_matching_for_incremental_indexer_roots() {
        assert!(is_same_or_ancestor(
            Path::new(r"C:\Users\Alice"),
            Path::new(r"C:\Users\Alice\Documents\Projects")
        ));
        assert!(is_same_or_ancestor(
            Path::new(r"c:/users/alice/"),
            Path::new(r"C:\Users\Alice")
        ));
        assert!(!is_same_or_ancestor(
            Path::new(r"C:\Users\Alice"),
            Path::new(r"C:\Users\Bob")
        ));

        let exclusions = vec![PathBuf::from(r"C:\Windows"), PathBuf::from("node_modules")];
        assert!(is_root_excluded(
            Path::new(r"C:\Windows\System32"),
            &exclusions
        ));
        assert!(is_root_excluded(
            Path::new(r"D:\App\node_modules\pkg"),
            &exclusions
        ));
        assert!(!is_root_excluded(Path::new(r"D:\App\src"), &exclusions));
    }
}
