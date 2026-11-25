//! Background filesystem watcher and debounced incremental updater for [`SearchIndex`].

use crate::index::SearchIndex;
use crate::types::{
    canonical_db_path, file_entry_from_metadata, is_safe_traversable_dir, path_starts_with_dir,
};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use onyx_core::{OnyxError, OnyxResult, to_extended_length_path};
use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc as std_mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Default debounce window for batching rapid filesystem events before updating the SQLite index.
pub const DEFAULT_INDEXER_DEBOUNCE: Duration = Duration::from_millis(75);

enum IndexerCommand {
    FsEvents(Vec<PathBuf>),
    Reconcile(PathBuf),
    Shutdown,
}

/// Background incremental updater that watches registered root directories via `notify`,
/// debounces create/modify/rename/remove events, and applies incremental `upsert_path` /
/// `remove_path` updates to a shared [`SearchIndex`].
pub struct IncrementalIndexer {
    index: Arc<Mutex<SearchIndex>>,
    excluded_dirs: Vec<PathBuf>,
    watcher: Mutex<Option<RecommendedWatcher>>,
    watched_roots: Arc<Mutex<HashSet<PathBuf>>>,
    cmd_tx: Mutex<Option<std_mpsc::Sender<IndexerCommand>>>,
    cancel: CancellationToken,
    worker_thread: Mutex<Option<JoinHandle<()>>>,
}

impl fmt::Debug for IncrementalIndexer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IncrementalIndexer")
            .field("excluded_dirs", &self.excluded_dirs)
            .field("watched_roots", &self.watched_roots())
            .finish_non_exhaustive()
    }
}

impl IncrementalIndexer {
    /// Starts a background incremental indexer for `index` with [`DEFAULT_INDEXER_DEBOUNCE`].
    pub fn start(index: Arc<Mutex<SearchIndex>>, excluded_dirs: Vec<PathBuf>) -> OnyxResult<Self> {
        Self::start_with_debounce(index, excluded_dirs, DEFAULT_INDEXER_DEBOUNCE)
    }

    /// Starts a background incremental indexer with a custom debounce interval.
    pub fn start_with_debounce(
        index: Arc<Mutex<SearchIndex>>,
        mut excluded_dirs: Vec<PathBuf>,
        debounce: Duration,
    ) -> OnyxResult<Self> {
        // Synchronize excluded directories between the shared SearchIndex and the background worker.
        if let Ok(mut guard) = index.lock() {
            for existing in guard.excluded_dirs() {
                if !excluded_dirs.contains(existing) {
                    excluded_dirs.push(existing.clone());
                }
            }
            for dir in &excluded_dirs {
                if !guard.excluded_dirs().contains(dir) {
                    guard.add_excluded_dir(dir.clone());
                }
            }
        }

        let (cmd_tx, cmd_rx) = std_mpsc::channel::<IndexerCommand>();
        let event_tx = cmd_tx.clone();
        let cancel = CancellationToken::new();

        let watcher = notify::recommended_watcher(move |res: notify::Result<Event>| match res {
            Ok(event) => {
                // Ignore pure read/access notifications that do not mutate metadata or content.
                if matches!(event.kind, EventKind::Access(_)) {
                    return;
                }
                if !event.paths.is_empty() {
                    let _ = event_tx.send(IndexerCommand::FsEvents(event.paths));
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "IncrementalIndexer filesystem watcher error");
            }
        })
        .map_err(|err| OnyxError::Search {
            message: format!("failed to initialize incremental index watcher: {err}"),
        })?;

        let watched_roots = Arc::new(Mutex::new(HashSet::<PathBuf>::new()));
        let worker_index = Arc::clone(&index);
        let worker_roots = Arc::clone(&watched_roots);
        let worker_excluded = excluded_dirs.clone();
        let worker_cancel = cancel.clone();

        let worker_thread = thread::Builder::new()
            .name("onyx-search-incremental-indexer".to_string())
            .spawn(move || {
                run_indexer_loop(
                    cmd_rx,
                    worker_index,
                    worker_roots,
                    worker_excluded,
                    debounce,
                    worker_cancel,
                );
            })
            .map_err(|err| OnyxError::Search {
                message: format!("failed to spawn incremental indexer worker thread: {err}"),
            })?;

        Ok(Self {
            index,
            excluded_dirs,
            watcher: Mutex::new(Some(watcher)),
            watched_roots,
            cmd_tx: Mutex::new(Some(cmd_tx)),
            cancel,
            worker_thread: Mutex::new(Some(worker_thread)),
        })
    }

    /// Registers `root` for recursive filesystem watching.
    ///
    /// No-op if `root` is already watched.
    pub fn watch_root(&self, root: PathBuf) -> OnyxResult<()> {
        let root_ext = to_extended_length_path(&root);
        let meta = match std::fs::symlink_metadata(&root_ext) {
            Ok(m) => m,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(OnyxError::NotFound { path: root });
            }
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(OnyxError::PermissionDenied { path: root });
            }
            Err(err) => return Err(OnyxError::Io(err)),
        };

        if !meta.is_dir() {
            return Err(OnyxError::InvalidPath {
                message: format!("watch root is not a directory: {}", root.display()),
            });
        }

        let canonical_key = PathBuf::from(canonical_db_path(&root));
        {
            let roots_guard = lock_unpoisoned(&self.watched_roots);
            if roots_guard.contains(&canonical_key) {
                return Ok(());
            }
        }

        {
            let mut watcher_guard = lock_unpoisoned(&self.watcher);
            if let Some(watcher) = watcher_guard.as_mut() {
                watcher
                    .watch(&root, RecursiveMode::Recursive)
                    .map_err(|err| OnyxError::Search {
                        message: format!("failed to watch root {}: {err}", root.display()),
                    })?;
            } else {
                return Err(OnyxError::Cancelled);
            }
        }

        let mut roots_guard = lock_unpoisoned(&self.watched_roots);
        roots_guard.insert(canonical_key);
        Ok(())
    }

    /// Stops watching `root`. Gracefully no-ops if `root` is not watched or has disappeared.
    pub fn unwatch_root(&self, root: &Path) {
        let canonical_key = PathBuf::from(canonical_db_path(root));
        let was_watched = {
            let mut roots_guard = lock_unpoisoned(&self.watched_roots);
            roots_guard.remove(&canonical_key) | roots_guard.remove(root)
        };
        if !was_watched {
            return;
        }

        let mut watcher_guard = lock_unpoisoned(&self.watcher);
        if let Some(watcher) = watcher_guard.as_mut()
            && let Err(err) = watcher.unwatch(root)
        {
            tracing::debug!(root = %root.display(), error = %err, "unwatch_root ignored error");
        }
    }

    /// Asynchronously triggers a full reconciliation of `root` on the background worker thread.
    pub fn trigger_reconcile(&self, root: PathBuf) {
        let tx_guard = lock_unpoisoned(&self.cmd_tx);
        if let Some(tx) = tx_guard.as_ref() {
            let _ = tx.send(IndexerCommand::Reconcile(root));
        }
    }

    /// Returns a snapshot of currently watched root directories.
    #[must_use]
    pub fn watched_roots(&self) -> Vec<PathBuf> {
        let roots_guard = lock_unpoisoned(&self.watched_roots);
        roots_guard.iter().cloned().collect()
    }

    /// Returns a reference to the shared [`SearchIndex`].
    #[must_use]
    pub fn index(&self) -> &Arc<Mutex<SearchIndex>> {
        &self.index
    }

    /// Stops watching all roots, cancels in-flight indexing work, and joins the background thread.
    pub fn shutdown(mut self) {
        self.shutdown_inner();
    }

    fn shutdown_inner(&mut self) {
        self.cancel.cancel();

        // Unwatch all roots and drop the OS watcher first so no new notify events are queued.
        let roots: Vec<PathBuf> = {
            let mut guard = lock_unpoisoned(&self.watched_roots);
            guard.drain().collect()
        };
        {
            let mut watcher_guard = lock_unpoisoned(&self.watcher);
            if let Some(mut watcher) = watcher_guard.take() {
                for root in roots {
                    let _ = watcher.unwatch(&root);
                }
            }
        }

        // Signal the worker loop to exit and close the sender.
        {
            let mut tx_guard = lock_unpoisoned(&self.cmd_tx);
            if let Some(tx) = tx_guard.take() {
                let _ = tx.send(IndexerCommand::Shutdown);
            }
        }

        let mut thread_guard = lock_unpoisoned(&self.worker_thread);
        if let Some(handle) = thread_guard.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for IncrementalIndexer {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}

fn run_indexer_loop(
    cmd_rx: std_mpsc::Receiver<IndexerCommand>,
    index: Arc<Mutex<SearchIndex>>,
    watched_roots: Arc<Mutex<HashSet<PathBuf>>>,
    excluded_dirs: Vec<PathBuf>,
    debounce: Duration,
    cancel: CancellationToken,
) {
    let mut pending_paths = HashSet::<PathBuf>::new();
    let mut pending_reconciles = VecDeque::<PathBuf>::new();

    while !cancel.is_cancelled() {
        let first = match cmd_rx.recv() {
            Ok(cmd) => cmd,
            Err(_) => break,
        };

        let mut should_shutdown = false;
        match first {
            IndexerCommand::FsEvents(paths) => {
                for p in paths {
                    pending_paths.insert(p);
                }
            }
            IndexerCommand::Reconcile(root) => {
                if !pending_reconciles.contains(&root) {
                    pending_reconciles.push_back(root);
                }
            }
            IndexerCommand::Shutdown => break,
        }

        // Coalesce subsequent events within the debounce window.
        let deadline = Instant::now() + debounce;
        while !cancel.is_cancelled() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match cmd_rx.recv_timeout(deadline.saturating_duration_since(now)) {
                Ok(IndexerCommand::FsEvents(paths)) => {
                    for p in paths {
                        pending_paths.insert(p);
                    }
                }
                Ok(IndexerCommand::Reconcile(root)) => {
                    if !pending_reconciles.contains(&root) {
                        pending_reconciles.push_back(root);
                    }
                }
                Ok(IndexerCommand::Shutdown) | Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                    should_shutdown = true;
                    break;
                }
                Err(std_mpsc::RecvTimeoutError::Timeout) => break,
            }
        }

        if should_shutdown || cancel.is_cancelled() {
            break;
        }

        if !pending_paths.is_empty() {
            let batch: Vec<PathBuf> = pending_paths.drain().collect();
            apply_debounced_paths(&index, &watched_roots, &excluded_dirs, &batch, &cancel);
        }

        while let Some(root) = pending_reconciles.pop_front() {
            if cancel.is_cancelled() {
                break;
            }
            apply_reconcile(&index, &root, &cancel);
        }
    }
}

fn apply_debounced_paths(
    index: &Arc<Mutex<SearchIndex>>,
    watched_roots: &Arc<Mutex<HashSet<PathBuf>>>,
    excluded_dirs: &[PathBuf],
    paths: &[PathBuf],
    cancel: &CancellationToken,
) {
    let roots_snapshot: HashSet<String> = {
        let guard = lock_unpoisoned(watched_roots);
        guard.iter().map(|p| canonical_db_path(p)).collect()
    };

    for path in paths {
        if cancel.is_cancelled() {
            return;
        }

        if is_path_excluded(path, excluded_dirs) {
            continue;
        }

        let canonical = canonical_db_path(path);
        let is_watched_root = roots_snapshot.iter().any(|r| {
            if cfg!(windows) {
                r.eq_ignore_ascii_case(&canonical)
            } else {
                r == &canonical
            }
        });

        let ext_path = to_extended_length_path(path);
        match std::fs::symlink_metadata(&ext_path) {
            Ok(meta) => {
                if is_watched_root {
                    // Watched root directory mtime changed when a child changed; child events handle the child.
                    continue;
                }
                if !is_safe_traversable_dir(&meta) {
                    // Build FileEntry outside the SearchIndex lock to keep lock hold times minimal.
                    let entry = file_entry_from_metadata(path, &meta);
                    if let Some(guard) = lock_index_graceful(index)
                        && let Err(err) = guard.upsert_entry(&entry)
                    {
                        tracing::debug!(path = %path.display(), error = %err, "IncrementalIndexer upsert_entry failed");
                    }
                } else if let Some(guard) = lock_index_graceful(index) {
                    if let Err(err) = guard.upsert_path(path) {
                        tracing::debug!(path = %path.display(), error = %err, "IncrementalIndexer dir upsert_path failed");
                    }
                    if let Err(err) = guard.reconcile_directory_with_cancel(path, cancel) {
                        tracing::debug!(path = %path.display(), error = %err, "IncrementalIndexer dir reconcile failed");
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if let Some(guard) = lock_index_graceful(index)
                    && let Err(db_err) = guard.remove_path(path)
                {
                    tracing::debug!(path = %path.display(), error = %db_err, "IncrementalIndexer remove_path failed");
                }
            }
            Err(err) => {
                tracing::debug!(
                    path = %path.display(),
                    error = %err,
                    "IncrementalIndexer skipping inaccessible path"
                );
            }
        }
    }
}

fn apply_reconcile(index: &Arc<Mutex<SearchIndex>>, root: &Path, cancel: &CancellationToken) {
    if cancel.is_cancelled() {
        return;
    }

    let root_ext = to_extended_length_path(root);
    let root_exists = std::fs::symlink_metadata(&root_ext).is_ok();

    if let Some(guard) = lock_index_graceful(index) {
        if !root_exists {
            if let Err(err) = guard.remove_path(root) {
                tracing::debug!(root = %root.display(), error = %err, "IncrementalIndexer remove missing root failed");
            }
            return;
        }

        if let Err(err) = guard.reconcile_directory_with_cancel(root, cancel) {
            tracing::debug!(root = %root.display(), error = %err, "IncrementalIndexer reconcile_directory failed");
        }
    }
}

fn is_path_excluded(path: &Path, excluded_dirs: &[PathBuf]) -> bool {
    if excluded_dirs.is_empty() {
        return false;
    }
    let candidate = canonical_db_path(path);

    for excluded in excluded_dirs {
        if excluded.is_absolute() {
            let excluded_str = canonical_db_path(excluded);
            if path_starts_with_dir(&candidate, &excluded_str) {
                return true;
            }
        } else {
            let excluded_name = excluded.to_string_lossy();
            for comp in path.components() {
                if let Component::Normal(os_str) = comp {
                    let comp_str = os_str.to_string_lossy();
                    #[cfg(windows)]
                    let matches = comp_str.eq_ignore_ascii_case(&excluded_name);
                    #[cfg(not(windows))]
                    let matches = comp_str == excluded_name.as_ref();
                    if matches {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn lock_index_graceful(index: &Arc<Mutex<SearchIndex>>) -> Option<MutexGuard<'_, SearchIndex>> {
    match index.lock() {
        Ok(guard) => Some(guard),
        Err(poisoned) => {
            tracing::warn!("SearchIndex mutex was poisoned; recovering inner guard");
            Some(poisoned.into_inner())
        }
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
