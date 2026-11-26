//! Indexed search for the GPUI prototype: the on-disk SQLite FTS5 index, the
//! debounced incremental watcher that keeps it current, and the per-run
//! generation/cancellation bookkeeping that drops stale results.
//!
//! Mirrors the lifecycle of the Slint app (`crates/onyx-app/src/content.rs`):
//! one shared [`SearchIndex`] over `cache/search.sqlite`, an
//! [`IncrementalIndexer`] that debounces watcher events into it, and
//! [`CancellationToken`]s for the indexer and for every individual search run.
//! Nothing here touches GPUI: the index lives on its own worker thread and every
//! query runs on the Tokio pool, so the UI thread only ever sends commands and
//! receives events.

use onyx_core::{FileEntry, GenerationId, OnyxError, OnyxResult};
use onyx_search::{
    IncrementalIndexer, SearchIndex, SearchQuery, SearchResult, UnifiedSearchEngine,
};
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Debounce before a keystroke becomes a search run. A newer keystroke cancels
/// the pending run, so only the last query in a burst reaches SQLite.
pub const SEARCH_DEBOUNCE: Duration = Duration::from_millis(180);

const CACHE_DIR: &str = "cache";
const INDEX_FILE_NAME: &str = "search.sqlite";
const INDEX_WORKER_THREAD: &str = "onyx-gpui-search-index";

/// Absolute path of the index database, derived from the config directory so
/// the index survives restarts next to `config.toml`.
#[must_use]
pub fn index_path(config_dir: &Path) -> PathBuf {
    config_dir.join(CACHE_DIR).join(INDEX_FILE_NAME)
}

/// Which source currently fills a file pane's list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListMode {
    /// The open folder, filtered in memory by the search box.
    #[default]
    Browse,
    /// Ranked hits of the current indexed search run.
    Search,
    /// Items of the Windows Recycle Bin, enumerated through the Shell namespace.
    /// That folder has no filesystem path, so the browse listing cannot fill it.
    RecycleBin,
}

/// State of one indexed search run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SearchStatus {
    /// No run in flight and no hits to show.
    #[default]
    Idle,
    /// A run is queued (debounce) or querying.
    Running,
    /// The last run finished with `matches` hits.
    Done { matches: usize },
    /// The last run failed; the message is shown in the status bar.
    Failed { message: String },
}

impl SearchStatus {
    /// Whether a run is still in flight, used for the LED activity pill.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Running)
    }
}

/// State of the background index for the status bar and settings overlay.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum IndexStatus {
    /// The index is off and no watcher runs.
    #[default]
    Disabled,
    /// The worker was asked to open the index.
    Opening,
    /// `index_directory` walks `root` on the blocking pool.
    Building { root: PathBuf },
    /// The index is open and the watcher applies debounced updates.
    /// `indexed` is 0 until a build reported its entry count.
    Watching { indexed: usize },
    /// Opening the index or the watcher failed.
    Failed { message: String },
}

impl IndexStatus {
    #[must_use]
    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Opening | Self::Building { .. })
    }

    /// Short label for the settings overlay and the status bar.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Disabled => "Index off".to_string(),
            Self::Opening => "Opening index…".to_string(),
            Self::Building { root } => {
                let name = root.file_name().map_or_else(
                    || root.display().to_string(),
                    |n| n.to_string_lossy().into(),
                );
                format!("Indexing {name}…")
            }
            Self::Watching { indexed: 0 } => "Index ready".to_string(),
            Self::Watching { indexed } => format!("Index: {indexed} items"),
            Self::Failed { message } => format!("Index error · {message}"),
        }
    }
}

/// One indexed search run: every keystroke opens a new generation with its own
/// token, and a finish is only accepted while that generation is current.
#[derive(Debug, Default)]
pub struct SearchRun {
    generation: GenerationId,
    cancel: CancellationToken,
    status: SearchStatus,
    query: String,
}

impl SearchRun {
    /// Cancels the run in flight and opens the next generation for `query`.
    pub fn restart(&mut self, query: String) -> (GenerationId, CancellationToken) {
        self.cancel.cancel();
        self.cancel = CancellationToken::new();
        self.generation = self.generation.next();
        self.status = SearchStatus::Running;
        self.query = query;
        (self.generation, self.cancel.clone())
    }

    /// Cancels the run in flight and returns to [`SearchStatus::Idle`].
    pub fn cancel(&mut self) {
        self.cancel.cancel();
        self.cancel = CancellationToken::new();
        self.generation = self.generation.next();
        self.status = SearchStatus::Idle;
        self.query.clear();
    }

    #[must_use]
    pub fn status(&self) -> &SearchStatus {
        &self.status
    }

    /// Marks the run as unusable without querying, for example because the
    /// index is switched off.
    pub fn fail(&mut self, message: String) {
        self.cancel.cancel();
        self.generation = self.generation.next();
        self.query.clear();
        self.status = SearchStatus::Failed { message };
    }

    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.status.is_busy()
    }

    /// Applies a finished run and returns its ranked hits. Results of a stale
    /// generation - a newer keystroke already restarted the run - are dropped
    /// without touching the current state.
    pub fn finish(
        &mut self,
        generation: GenerationId,
        result: OnyxResult<Vec<SearchResult>>,
    ) -> Option<Vec<FileEntry>> {
        if generation != self.generation {
            return None;
        }
        match result {
            Ok(results) => {
                let hits: Vec<FileEntry> = results.into_iter().map(|result| result.entry).collect();
                self.status = SearchStatus::Done {
                    matches: hits.len(),
                };
                Some(hits)
            }
            Err(OnyxError::Cancelled) => {
                self.status = SearchStatus::Idle;
                None
            }
            Err(error) => {
                self.status = SearchStatus::Failed {
                    message: error.to_string(),
                };
                None
            }
        }
    }
}

/// Runs one query with the shared debounce/cancel discipline. Cancelling the
/// token during the debounce returns [`OnyxError::Cancelled`] without touching
/// the index.
pub async fn run_query(
    engine: UnifiedSearchEngine,
    root: PathBuf,
    query: SearchQuery,
    cancel: CancellationToken,
    debounce: bool,
) -> OnyxResult<Vec<SearchResult>> {
    if debounce {
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(OnyxError::Cancelled),
            () = tokio::time::sleep(SEARCH_DEBOUNCE) => {}
        }
    }
    engine.search(root, query, cancel).await
}

/// Commands sent from the GPUI thread to the index worker. Sending never blocks.
#[derive(Debug)]
pub enum IndexCommand {
    /// Open the index if needed, then watch `roots`.
    Enable {
        roots: Vec<PathBuf>,
        exclusions: Vec<PathBuf>,
    },
    /// Replace the watched roots; the index and its database stay open.
    Watch { roots: Vec<PathBuf> },
    /// Close the index and stop the watcher.
    Disable,
    /// Stop the worker for good.
    Shutdown,
}

/// Events reported back to the GPUI thread.
#[derive(Debug)]
pub enum IndexEvent {
    /// The index is open and the debounced watcher is running.
    Ready { index: Arc<Mutex<SearchIndex>> },
    /// The index and its watcher were closed.
    Disabled,
    /// Opening the index or the watcher failed.
    Failed { message: String },
}

/// UI-side handle of the index worker. Owns the command channel and the
/// shutdown token; dropping it stops the worker without blocking the caller.
pub struct IndexController {
    commands: std_mpsc::Sender<IndexCommand>,
    path: PathBuf,
    cancel: CancellationToken,
    worker: Option<JoinHandle<()>>,
}

impl fmt::Debug for IndexController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexController")
            .field("path", &self.path)
            .field("cancelled", &self.cancel.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl IndexController {
    /// Starts the worker thread that owns the index and the watcher.
    pub fn start(
        path: PathBuf,
        shutdown: &CancellationToken,
        events: mpsc::UnboundedSender<IndexEvent>,
    ) -> Self {
        let (commands, inbox) = std_mpsc::channel();
        let cancel = shutdown.child_token();
        let worker_path = path.clone();
        let worker_cancel = cancel.clone();
        let worker = thread::Builder::new()
            .name(INDEX_WORKER_THREAD.to_string())
            .spawn(move || worker_loop(worker_path, inbox, events, worker_cancel))
            .ok();
        Self {
            commands,
            path,
            cancel,
            worker,
        }
    }

    /// Opens the index and starts watching `roots`.
    pub fn enable(&self, roots: Vec<PathBuf>, exclusions: Vec<PathBuf>) {
        self.send(IndexCommand::Enable { roots, exclusions });
    }

    /// Replaces the watched roots of the already open index.
    pub fn watch(&self, roots: Vec<PathBuf>) {
        self.send(IndexCommand::Watch { roots });
    }

    /// Closes the index and stops the watcher.
    pub fn disable(&self) {
        self.send(IndexCommand::Disable);
    }

    fn send(&self, command: IndexCommand) {
        // A closed channel means the worker already stopped; dropping the
        // command keeps the UI thread responsive during shutdown.
        let _ = self.commands.send(command);
    }
}

impl Drop for IndexController {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.send(IndexCommand::Shutdown);
        // The worker joins the incremental indexer itself; detaching keeps the
        // closing UI thread waiting for nothing.
        let _ = self.worker.take();
    }
}

fn worker_loop(
    path: PathBuf,
    inbox: std_mpsc::Receiver<IndexCommand>,
    events: mpsc::UnboundedSender<IndexEvent>,
    cancel: CancellationToken,
) {
    let mut session = IndexSession::default();
    while !cancel.is_cancelled() {
        let Ok(command) = inbox.recv() else {
            break;
        };
        match command {
            IndexCommand::Enable { roots, exclusions } => {
                if session.open(&path, exclusions, &events) {
                    session.watch(&roots, &events);
                }
            }
            IndexCommand::Watch { roots } => {
                if session.is_open() {
                    session.watch(&roots, &events);
                }
            }
            IndexCommand::Disable => {
                session.close();
                let _ = events.send(IndexEvent::Disabled);
            }
            IndexCommand::Shutdown => break,
        }
    }
    session.close();
}

/// Worker-side state: the open index handle and the roots it watches.
#[derive(Default)]
struct IndexSession {
    indexer: Option<IncrementalIndexer>,
    watched: HashSet<PathBuf>,
}

impl IndexSession {
    fn is_open(&self) -> bool {
        self.indexer.is_some()
    }

    /// Opens the database and starts the debounced watcher. Returns whether the
    /// index is usable; failures are reported to the UI thread.
    fn open(
        &mut self,
        path: &Path,
        exclusions: Vec<PathBuf>,
        events: &mpsc::UnboundedSender<IndexEvent>,
    ) -> bool {
        if self.indexer.is_some() {
            return true;
        }
        let shared = match SearchIndex::open(path) {
            Ok(index) => Arc::new(Mutex::new(index.with_excluded_dirs(exclusions.clone()))),
            Err(error) => {
                let _ = events.send(IndexEvent::Failed {
                    message: format!("could not open search index: {error}"),
                });
                return false;
            }
        };
        match IncrementalIndexer::start(Arc::clone(&shared), exclusions) {
            Ok(indexer) => {
                self.indexer = Some(indexer);
                self.watched.clear();
                let _ = events.send(IndexEvent::Ready { index: shared });
                true
            }
            Err(error) => {
                let _ = events.send(IndexEvent::Failed {
                    message: format!("could not start search index watcher: {error}"),
                });
                false
            }
        }
    }

    /// Synchronizes the watched roots with `roots`, unwatching everything else.
    fn watch(&mut self, roots: &[PathBuf], events: &mpsc::UnboundedSender<IndexEvent>) {
        let Some(indexer) = self.indexer.as_ref() else {
            return;
        };
        let wanted: HashSet<PathBuf> = roots
            .iter()
            .filter(|root| !root.as_os_str().is_empty())
            .cloned()
            .collect();
        self.watched.retain(|root| {
            if wanted.contains(root) {
                true
            } else {
                indexer.unwatch_root(root);
                false
            }
        });
        for root in wanted {
            if self.watched.contains(&root) {
                continue;
            }
            match indexer.watch_root(root.clone()) {
                Ok(()) => {
                    self.watched.insert(root);
                }
                Err(error) => {
                    let _ = events.send(IndexEvent::Failed {
                        message: format!("could not watch {}: {error}", root.display()),
                    });
                }
            }
        }
    }

    /// Stops the watcher and closes the database on the worker thread.
    fn close(&mut self) {
        if let Some(indexer) = self.indexer.take() {
            indexer.shutdown();
        }
        self.watched.clear();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn build_sample_tree() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("report-2026.txt"), "quarterly").unwrap();
        fs::write(root.join("notes.md"), "# Notes").unwrap();
        let nested = root.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("deep-report.txt"), "nested report").unwrap();
        dir
    }

    fn query(text: &str) -> SearchQuery {
        SearchQuery {
            text: text.to_string(),
            ..SearchQuery::default()
        }
    }

    fn hit(path: &str, name: &str) -> SearchResult {
        SearchResult {
            entry: FileEntry {
                path: PathBuf::from(path),
                name: name.to_string(),
                extension: None,
                kind: onyx_core::FileKind::File,
                size: 0,
                modified: None,
                created: None,
                attributes: onyx_core::FileAttributes::default(),
                is_symlink: false,
                symlink_target: None,
                type_label: "File".to_string(),
            },
            score: 90.0,
            source: onyx_search::SearchSource::SqliteIndex,
        }
    }

    #[test]
    fn index_path_keeps_the_database_next_to_the_config_cache() {
        let path = index_path(Path::new(r"C:\Users\onyx\AppData\Roaming\onyx"));
        assert!(path.ends_with(Path::new("cache").join("search.sqlite")));
        assert_eq!(
            path.parent().and_then(Path::file_name),
            Some(std::ffi::OsStr::new("cache"))
        );
    }

    #[test]
    fn restart_opens_a_new_generation_and_cancels_the_previous_run() {
        let mut run = SearchRun::default();
        let (first_generation, first_cancel) = run.restart("alpha".to_string());
        let (second_generation, second_cancel) = run.restart("alpha be".to_string());

        assert!(first_cancel.is_cancelled());
        assert!(!second_cancel.is_cancelled());
        assert!(second_generation.value() > first_generation.value());
        assert_eq!(run.status(), &SearchStatus::Running);
        assert!(run.is_busy());
    }

    #[test]
    fn stale_generation_is_dropped_without_touching_the_current_state() {
        let mut run = SearchRun::default();
        let stale_generation = run.restart("alpha".to_string()).0;
        let current_generation = run.restart("alphabet".to_string()).0;

        let dropped = run.finish(
            stale_generation,
            Ok(vec![hit(r"C:\data\alphabet.rs", "alphabet.rs")]),
        );
        assert!(dropped.is_none());
        assert_eq!(run.status(), &SearchStatus::Running);

        let hits = run
            .finish(
                current_generation,
                Ok(vec![hit(r"C:\data\alphabet.rs", "alphabet.rs")]),
            )
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(run.status(), &SearchStatus::Done { matches: 1 });
    }

    #[test]
    fn cancelled_run_reports_idle_instead_of_an_error() {
        let mut run = SearchRun::default();
        let (generation, _) = run.restart("alpha".to_string());
        assert!(run.finish(generation, Err(OnyxError::Cancelled)).is_none());
        assert_eq!(run.status(), &SearchStatus::Idle);
    }

    #[test]
    fn failed_run_keeps_the_message_for_the_status_bar() {
        let mut run = SearchRun::default();
        let (generation, _) = run.restart("alpha".to_string());
        let result = run.finish(
            generation,
            Err(OnyxError::Search {
                message: "index unavailable".to_string(),
            }),
        );
        assert!(result.is_none());
        assert!(matches!(
            run.status(),
            SearchStatus::Failed { message } if message.contains("index unavailable")
        ));
    }

    #[test]
    fn cancelled_debounce_never_reaches_the_index() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = runtime.block_on(run_query(
            UnifiedSearchEngine::live_only(),
            PathBuf::from(r"C:\does\not\matter"),
            query("alpha"),
            cancel,
            true,
        ));
        assert!(matches!(result, Err(OnyxError::Cancelled)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_disk_index_answers_queries_after_reopen() {
        let tree = build_sample_tree();
        let db_dir = TempDir::new().unwrap();
        let db_path = index_path(db_dir.path());

        {
            let index = SearchIndex::open(&db_path).unwrap();
            let cancel = CancellationToken::new();
            let indexed = index.index_directory(tree.path(), &cancel).unwrap();
            assert!(indexed >= 3);
        }

        // A fresh process would open the same file; the hits must survive.
        let reopened = Arc::new(Mutex::new(SearchIndex::open(&db_path).unwrap()));
        let engine = UnifiedSearchEngine::with_index(Arc::clone(&reopened));
        let hits = engine
            .search(
                tree.path().to_path_buf(),
                query("report"),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert!(
            hits.iter()
                .all(|hit| hit.source == onyx_search::SearchSource::SqliteIndex)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn index_controller_opens_the_index_and_shuts_down_cleanly() {
        let tree = build_sample_tree();
        let db_dir = TempDir::new().unwrap();
        let shutdown = CancellationToken::new();
        let (events, mut received) = mpsc::unbounded_channel();

        let controller = IndexController::start(index_path(db_dir.path()), &shutdown, events);
        controller.enable(vec![tree.path().to_path_buf()], Vec::new());

        let event = tokio::time::timeout(Duration::from_secs(10), received.recv())
            .await
            .expect("index worker must report ready")
            .expect("event channel must stay open");
        let IndexEvent::Ready { index } = event else {
            panic!("expected Ready, got {event:?}");
        };

        // The index opened by the worker is usable from the UI side.
        let cancel = CancellationToken::new();
        index
            .lock()
            .unwrap()
            .index_directory(tree.path(), &cancel)
            .unwrap();
        let engine = UnifiedSearchEngine::with_index(Arc::clone(&index));
        let hits = engine
            .search(
                tree.path().to_path_buf(),
                query("notes"),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);

        controller.disable();
        let disabled = tokio::time::timeout(Duration::from_secs(10), received.recv())
            .await
            .expect("index worker must report the shutdown")
            .expect("event channel must stay open");
        assert!(matches!(disabled, IndexEvent::Disabled));

        // Dropping the controller stops the worker; the event channel then closes.
        drop(controller);
        let closed = tokio::time::timeout(Duration::from_secs(10), received.recv()).await;
        assert!(
            matches!(closed, Ok(None)),
            "worker must exit and drop `events`"
        );
    }

    #[test]
    fn status_labels_cover_every_state() {
        assert_eq!(IndexStatus::Disabled.label(), "Index off");
        assert_eq!(IndexStatus::Watching { indexed: 0 }.label(), "Index ready");
        assert_eq!(
            IndexStatus::Watching { indexed: 50_000 }.label(),
            "Index: 50000 items"
        );
        assert_eq!(
            IndexStatus::Building {
                root: PathBuf::from(r"C:\data")
            }
            .label(),
            "Indexing data…"
        );
        assert!(
            IndexStatus::Failed {
                message: "boom".to_string()
            }
            .label()
            .contains("boom")
        );
        assert!(!IndexStatus::Watching { indexed: 1 }.is_busy());
        assert!(
            IndexStatus::Building {
                root: PathBuf::from(r"C:\data")
            }
            .is_busy()
        );
    }
}
