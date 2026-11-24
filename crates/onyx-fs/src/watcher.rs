use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use onyx_core::{OnyxError, OnyxResult};
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc as std_mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;
use tracing::warn;

/// Default debounce window for filesystem change notifications.
pub const DEFAULT_DEBOUNCE_DURATION: Duration = Duration::from_millis(150);

/// A debounced filesystem change notification for a watched directory.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WatchEvent {
    pub path: PathBuf,
}

/// Watches one or more directories and emits debounced [`WatchEvent`] notifications.
/// Stops watching and shuts down its debouncer thread cleanly when dropped.
pub struct DirectoryWatcher {
    watcher: Option<RecommendedWatcher>,
    watched: Arc<Mutex<HashSet<PathBuf>>>,
    debounce_thread: Option<JoinHandle<()>>,
}

impl DirectoryWatcher {
    /// Creates a `DirectoryWatcher` with [`DEFAULT_DEBOUNCE_DURATION`].
    pub fn new(tx: UnboundedSender<WatchEvent>) -> OnyxResult<Self> {
        Self::with_debounce(tx, DEFAULT_DEBOUNCE_DURATION)
    }

    /// Creates a `DirectoryWatcher` with a custom debounce interval.
    pub fn with_debounce(tx: UnboundedSender<WatchEvent>, debounce: Duration) -> OnyxResult<Self> {
        let (raw_tx, raw_rx) = std_mpsc::channel::<PathBuf>();
        let watched = Arc::new(Mutex::new(HashSet::<PathBuf>::new()));
        let watched_for_cb = Arc::clone(&watched);

        let watcher = notify::recommended_watcher(move |res: notify::Result<Event>| match res {
            Ok(event) => {
                let watched_snapshot: Vec<PathBuf> = {
                    let guard = lock_unpoisoned(&watched_for_cb);
                    guard.iter().cloned().collect()
                };
                for event_path in event.paths {
                    let resolved = resolve_watched_dir(&event_path, &watched_snapshot);
                    let _ = raw_tx.send(resolved);
                }
            }
            Err(err) => {
                warn!(error = %err, "Filesystem watcher error");
            }
        })
        .map_err(|err| OnyxError::OperationFailed {
            message: format!("failed to initialize directory watcher: {err}"),
        })?;

        let debounce_thread = thread::Builder::new()
            .name("onyx-fs-watcher-debounce".to_string())
            .spawn(move || run_debounce_loop(raw_rx, tx, debounce))
            .map_err(|err| OnyxError::OperationFailed {
                message: format!("failed to spawn watcher debounce thread: {err}"),
            })?;

        Ok(Self {
            watcher: Some(watcher),
            watched,
            debounce_thread: Some(debounce_thread),
        })
    }

    /// Starts watching `path` (non-recursively). No-op if already watched.
    pub fn watch(&mut self, path: &Path) -> OnyxResult<()> {
        let normalized = path.to_path_buf();
        {
            let guard = lock_unpoisoned(&self.watched);
            if guard.contains(&normalized) {
                return Ok(());
            }
        }

        if let Some(watcher) = self.watcher.as_mut() {
            watcher
                .watch(&normalized, RecursiveMode::NonRecursive)
                .map_err(|err| OnyxError::OperationFailed {
                    message: format!("failed to watch {}: {err}", normalized.display()),
                })?;
        }

        let mut guard = lock_unpoisoned(&self.watched);
        guard.insert(normalized);
        Ok(())
    }

    /// Stops watching `path`. No-op if not currently watched.
    pub fn unwatch(&mut self, path: &Path) -> OnyxResult<()> {
        let was_watched = {
            let mut guard = lock_unpoisoned(&self.watched);
            guard.remove(path)
        };
        if !was_watched {
            return Ok(());
        }

        if let Some(watcher) = self.watcher.as_mut() {
            watcher
                .unwatch(path)
                .map_err(|err| OnyxError::OperationFailed {
                    message: format!("failed to unwatch {}: {err}", path.display()),
                })?;
        }
        Ok(())
    }

    /// Replaces the current watch set with a single directory `path`.
    pub fn switch_path(&mut self, path: &Path) -> OnyxResult<()> {
        self.set_watched_paths(&[path.to_path_buf()])
    }

    /// Synchronizes the watched directories so that exactly `paths` are watched.
    pub fn set_watched_paths(&mut self, paths: &[PathBuf]) -> OnyxResult<()> {
        let desired: HashSet<PathBuf> = paths.iter().cloned().collect();
        let current: Vec<PathBuf> = {
            let guard = lock_unpoisoned(&self.watched);
            guard.iter().cloned().collect()
        };

        for existing in current {
            if !desired.contains(&existing) {
                let _ = self.unwatch(&existing);
            }
        }

        for path in paths {
            self.watch(path)?;
        }

        Ok(())
    }

    /// Stops watching all currently watched directories.
    pub fn unwatch_all(&mut self) {
        let current: Vec<PathBuf> = {
            let mut guard = lock_unpoisoned(&self.watched);
            guard.drain().collect()
        };
        if let Some(watcher) = self.watcher.as_mut() {
            for path in current {
                let _ = watcher.unwatch(&path);
            }
        }
    }

    /// Returns a snapshot of currently watched directory paths.
    #[must_use]
    pub fn watched_paths(&self) -> Vec<PathBuf> {
        let guard = lock_unpoisoned(&self.watched);
        guard.iter().cloned().collect()
    }
}

impl Drop for DirectoryWatcher {
    fn drop(&mut self) {
        self.unwatch_all();
        // Dropping the notify watcher closes `raw_tx`, allowing the debounce thread to exit.
        let _ = self.watcher.take();
        if let Some(handle) = self.debounce_thread.take() {
            let _ = handle.join();
        }
    }
}

impl fmt::Debug for DirectoryWatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectoryWatcher")
            .field("watched_paths", &self.watched_paths())
            .finish()
    }
}

fn resolve_watched_dir(event_path: &Path, watched_dirs: &[PathBuf]) -> PathBuf {
    for dir in watched_dirs {
        if event_path == dir || event_path.starts_with(dir) {
            return dir.clone();
        }
    }
    event_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| event_path.to_path_buf())
}

fn run_debounce_loop(
    raw_rx: std_mpsc::Receiver<PathBuf>,
    tx: UnboundedSender<WatchEvent>,
    debounce: Duration,
) {
    let mut pending = HashSet::<PathBuf>::new();

    while let Ok(first) = raw_rx.recv() {
        pending.insert(first);
        let deadline = Instant::now() + debounce;

        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match raw_rx.recv_timeout(deadline.saturating_duration_since(now)) {
                Ok(path) => {
                    pending.insert(path);
                }
                Err(std_mpsc::RecvTimeoutError::Timeout) => break,
                Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                    for path in pending.drain() {
                        let _ = tx.send(WatchEvent { path });
                    }
                    return;
                }
            }
        }

        for path in pending.drain() {
            if tx.send(WatchEvent { path }).is_err() {
                return;
            }
        }
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn watcher_emits_debounced_event_and_switches_paths() {
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut watcher = DirectoryWatcher::with_debounce(tx, Duration::from_millis(50)).unwrap();
        watcher.watch(dir_a.path()).unwrap();
        assert_eq!(watcher.watched_paths().len(), 1);

        // Trigger multiple rapid writes in dir_a -> should coalesce cleanly.
        fs::write(dir_a.path().join("one.txt"), b"1").unwrap();
        fs::write(dir_a.path().join("two.txt"), b"2").unwrap();

        let event = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("timed out waiting for watch event")
            .expect("watcher channel closed unexpectedly");
        assert_eq!(event.path, dir_a.path());

        // Switch to dir_b and verify events come from dir_b.
        watcher.switch_path(dir_b.path()).unwrap();
        assert_eq!(watcher.watched_paths(), vec![dir_b.path().to_path_buf()]);

        // Drain any lingering buffered event from dir_a before mutating dir_b.
        while rx.try_recv().is_ok() {}

        fs::write(dir_b.path().join("three.txt"), b"3").unwrap();
        let event_b = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("timed out waiting for dir_b event")
            .expect("watcher channel closed unexpectedly");
        assert_eq!(event_b.path, dir_b.path());

        drop(watcher);
    }
}
