use crate::types::{
    CompiledQuery, SearchQuery, SearchResult, SearchSource, file_entry_from_metadata,
    is_safe_traversable_dir, sort_search_results,
};
use onyx_core::{OnyxError, OnyxResult, to_extended_length_path};
use std::collections::VecDeque;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const DEFAULT_BATCH_SIZE: usize = 64;
const CANCEL_CHECK_INTERVAL: usize = 64;

/// Phase A non-blocking filesystem traversal search engine.
#[derive(Debug, Clone)]
pub struct LiveSearchEngine {
    batch_size: usize,
}

impl Default for LiveSearchEngine {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }
}

impl LiveSearchEngine {
    /// Creates a new live search engine with the default streaming batch size.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a new live search engine with a custom streaming batch size (clamped to >= 1).
    #[must_use]
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self {
            batch_size: batch_size.max(1),
        }
    }

    /// Recursively walks `root` on a blocking worker pool and streams matched batches over `tx`.
    ///
    /// Returns the total number of results emitted before reaching `query.max_results` or
    /// exhausting the directory tree.
    pub async fn search_streaming(
        &self,
        root: PathBuf,
        query: SearchQuery,
        cancel: CancellationToken,
        tx: mpsc::Sender<Vec<SearchResult>>,
    ) -> OnyxResult<usize> {
        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        let batch_size = self.batch_size;
        let compiled = CompiledQuery::from_query(&query);

        tokio::task::spawn_blocking(move || {
            walk_and_stream(root, &compiled, &cancel, batch_size, &tx)
        })
        .await
        .map_err(|err| OnyxError::Search {
            message: format!("live search worker failed: {err}"),
        })?
    }

    /// Recursively walks `root` and returns all matched results sorted by descending relevance.
    pub async fn search(
        &self,
        root: PathBuf,
        query: SearchQuery,
        cancel: CancellationToken,
    ) -> OnyxResult<Vec<SearchResult>> {
        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        let max_results = query.max_results;
        let (tx, mut rx) = mpsc::channel::<Vec<SearchResult>>(32);
        let engine = self.clone();
        let cancel_worker = cancel.clone();

        let worker = tokio::spawn(async move {
            engine
                .search_streaming(root, query, cancel_worker, tx)
                .await
        });

        let mut collected = Vec::new();
        while let Some(mut batch) = rx.recv().await {
            if cancel.is_cancelled() {
                return Err(OnyxError::Cancelled);
            }
            collected.append(&mut batch);
        }

        worker.await.map_err(|err| OnyxError::Search {
            message: format!("live search task failed: {err}"),
        })??;

        sort_search_results(&mut collected);
        collected.truncate(max_results);
        Ok(collected)
    }
}

fn walk_and_stream(
    root: PathBuf,
    query: &CompiledQuery,
    cancel: &CancellationToken,
    batch_size: usize,
    tx: &mpsc::Sender<Vec<SearchResult>>,
) -> OnyxResult<usize> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    let root_ext = to_extended_length_path(&root);
    let root_meta = match std::fs::symlink_metadata(&root_ext) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(OnyxError::NotFound { path: root });
        }
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(OnyxError::PermissionDenied { path: root });
        }
        Err(err) => return Err(OnyxError::Io(err)),
    };

    if !root_meta.is_dir() {
        return Err(OnyxError::InvalidPath {
            message: format!("search root is not a directory: {}", root.display()),
        });
    }

    if query.max_results == 0 {
        return Ok(0);
    }

    let mut queue = VecDeque::new();
    queue.push_back(root);

    let mut batch = Vec::with_capacity(batch_size.min(query.max_results));
    let mut total_emitted = 0usize;
    let mut scanned_entries = 0usize;

    while let Some(dir) = queue.pop_front() {
        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        let dir_ext = to_extended_length_path(&dir);
        let read_dir = match std::fs::read_dir(&dir_ext) {
            Ok(rd) => rd,
            Err(err) => {
                tracing::debug!(path = %dir.display(), error = %err, "skipping unreadable directory");
                continue;
            }
        };

        for dir_entry_res in read_dir {
            scanned_entries += 1;
            if scanned_entries.is_multiple_of(CANCEL_CHECK_INTERVAL) && cancel.is_cancelled() {
                return Err(OnyxError::Cancelled);
            }

            let Ok(dir_entry) = dir_entry_res else {
                continue;
            };

            let entry_path = dir.join(dir_entry.file_name());
            let entry_ext = to_extended_length_path(&entry_path);
            let Ok(meta) = std::fs::symlink_metadata(&entry_ext) else {
                continue;
            };

            let file_entry = file_entry_from_metadata(&entry_path, &meta);

            if !query.include_hidden && file_entry.attributes.hidden {
                continue;
            }

            // Descend only into real directories (never follow symlinks or Windows junctions/reparse points).
            if is_safe_traversable_dir(&meta) {
                queue.push_back(entry_path);
            }

            if let Some(score) = query.match_entry(&file_entry) {
                batch.push(SearchResult {
                    entry: file_entry,
                    score,
                    source: SearchSource::LiveWalk,
                });
                total_emitted += 1;

                if batch.len() >= batch_size || total_emitted >= query.max_results {
                    let to_send = std::mem::replace(
                        &mut batch,
                        Vec::with_capacity(batch_size.min(query.max_results)),
                    );
                    if tx.blocking_send(to_send).is_err() {
                        return Ok(total_emitted);
                    }
                }

                if total_emitted >= query.max_results {
                    return Ok(total_emitted);
                }
            }
        }
    }

    if !batch.is_empty() {
        let _ = tx.blocking_send(batch);
    }

    Ok(total_emitted)
}
