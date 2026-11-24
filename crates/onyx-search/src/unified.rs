use crate::index::SearchIndex;
use crate::live::LiveSearchEngine;
use crate::types::{SearchQuery, SearchResult};
use onyx_core::{OnyxError, OnyxResult};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Dual-mode search coordinator that prefers the SQLite FTS5 index when enabled and populated
/// for the requested root, gracefully falling back to [`LiveSearchEngine`].
#[derive(Clone, Default)]
pub struct UnifiedSearchEngine {
    index: Option<Arc<Mutex<SearchIndex>>>,
    live: LiveSearchEngine,
}

impl fmt::Debug for UnifiedSearchEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnifiedSearchEngine")
            .field("has_index", &self.index.is_some())
            .field("live", &self.live)
            .finish()
    }
}

impl UnifiedSearchEngine {
    /// Creates a unified engine with an optional shared SQLite search index.
    #[must_use]
    pub fn new(index: Option<Arc<Mutex<SearchIndex>>>) -> Self {
        Self {
            index,
            live: LiveSearchEngine::default(),
        }
    }

    /// Creates a unified engine backed solely by live filesystem traversal.
    #[must_use]
    pub fn live_only() -> Self {
        Self::new(None)
    }

    /// Creates a unified engine backed by a shared SQLite index with live-walk fallback.
    #[must_use]
    pub fn with_index(index: Arc<Mutex<SearchIndex>>) -> Self {
        Self::new(Some(index))
    }

    /// Replaces the live search engine configuration.
    #[must_use]
    pub fn with_live_engine(mut self, live: LiveSearchEngine) -> Self {
        self.live = live;
        self
    }

    /// Returns the attached SQLite search index, if any.
    #[must_use]
    pub fn index(&self) -> Option<&Arc<Mutex<SearchIndex>>> {
        self.index.as_ref()
    }

    /// Returns a reference to the live search engine.
    #[must_use]
    pub fn live_engine(&self) -> &LiveSearchEngine {
        &self.live
    }

    /// Executes a search under `root`, choosing SQLite FTS5 when `query.use_index` is true
    /// and the index contains entries under `root`, otherwise falling back to live traversal.
    pub async fn search(
        &self,
        root: PathBuf,
        query: SearchQuery,
        cancel: CancellationToken,
    ) -> OnyxResult<Vec<SearchResult>> {
        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        if query.use_index
            && let Some(index_arc) = self.index.clone()
        {
            let root_clone = root.clone();
            let query_clone = query.clone();
            let cancel_clone = cancel.clone();

            let index_outcome =
                tokio::task::spawn_blocking(move || -> OnyxResult<Option<Vec<SearchResult>>> {
                    if cancel_clone.is_cancelled() {
                        return Err(OnyxError::Cancelled);
                    }
                    let Ok(index) = index_arc.lock() else {
                        return Ok(None);
                    };
                    if !index.has_entries_under(&root_clone)? {
                        return Ok(None);
                    }
                    if cancel_clone.is_cancelled() {
                        return Err(OnyxError::Cancelled);
                    }
                    let results = index.query(Some(&root_clone), &query_clone)?;
                    Ok(Some(results))
                })
                .await
                .map_err(|err| OnyxError::Search {
                    message: format!("index query worker failed: {err}"),
                })?;

            match index_outcome {
                Ok(Some(results)) => return Ok(results),
                Ok(None) => {
                    tracing::debug!(root = %root.display(), "index has no entries under root; falling back to live walk");
                }
                Err(OnyxError::Cancelled) => return Err(OnyxError::Cancelled),
                Err(err) => {
                    tracing::warn!(error = %err, "index query failed; falling back to live walk");
                }
            }
        }

        self.live.search(root, query, cancel).await
    }

    /// Streaming variant of [`Self::search`], emitting matched batches over `tx`.
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

        if query.use_index
            && let Some(index_arc) = self.index.clone()
        {
            let root_clone = root.clone();
            let query_clone = query.clone();
            let cancel_clone = cancel.clone();

            let index_outcome =
                tokio::task::spawn_blocking(move || -> OnyxResult<Option<Vec<SearchResult>>> {
                    if cancel_clone.is_cancelled() {
                        return Err(OnyxError::Cancelled);
                    }
                    let Ok(index) = index_arc.lock() else {
                        return Ok(None);
                    };
                    if !index.has_entries_under(&root_clone)? {
                        return Ok(None);
                    }
                    let results = index.query(Some(&root_clone), &query_clone)?;
                    Ok(Some(results))
                })
                .await
                .map_err(|err| OnyxError::Search {
                    message: format!("index streaming worker failed: {err}"),
                })?;

            match index_outcome {
                Ok(Some(results)) => {
                    let count = results.len();
                    if !results.is_empty() {
                        let _ = tx.send(results).await;
                    }
                    return Ok(count);
                }
                Ok(None) => {}
                Err(OnyxError::Cancelled) => return Err(OnyxError::Cancelled),
                Err(err) => {
                    tracing::warn!(error = %err, "index query failed; falling back to live walk");
                }
            }
        }

        self.live.search_streaming(root, query, cancel, tx).await
    }
}
