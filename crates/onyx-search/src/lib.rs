//! Dual-mode streaming and SQLite FTS5 search engine for Onyx.

pub mod index;
pub mod live;
pub mod types;
pub mod unified;

pub use index::SearchIndex;
pub use live::LiveSearchEngine;
pub use types::{DEFAULT_MAX_RESULTS, IndexStats, SearchQuery, SearchResult, SearchSource};
pub use unified::UnifiedSearchEngine;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use onyx_core::{FileAttributes, FileEntry, FileKind, OnyxError};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    fn build_sample_tree() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        fs::write(root.join("alpha.rs"), "fn alpha() {}").unwrap();
        fs::write(
            root.join("alpha_beta.rs"),
            "fn alpha_beta() { /* longer content here */ }",
        )
        .unwrap();
        fs::write(root.join("notes.md"), "# Notes\nSome markdown content").unwrap();
        fs::write(root.join(".hidden_secret.txt"), "top secret").unwrap();

        let sub = root.join("nested");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("report-2026.final.txt"), "quarterly report").unwrap();
        fs::write(sub.join("big_file.bin"), vec![0u8; 4096]).unwrap();

        let ignored = root.join("node_modules");
        fs::create_dir(&ignored).unwrap();
        fs::write(ignored.join("alpha_dep.js"), "console.log(1);").unwrap();

        dir
    }

    #[tokio::test]
    async fn live_search_matches_tokens_and_ranks_exact_and_prefix() {
        let temp = build_sample_tree();
        let engine = LiveSearchEngine::new();
        let query = SearchQuery {
            text: "alpha".to_string(),
            ..SearchQuery::default()
        };

        let results = engine
            .search(temp.path().to_path_buf(), query, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|r| r.source == SearchSource::LiveWalk));
        // "alpha.rs" has exact stem match (90.0), higher than "alpha_beta.rs" / "alpha_dep.js"
        assert_eq!(results[0].entry.name, "alpha.rs");
        assert!(results[0].score > results[1].score);
    }

    #[tokio::test]
    async fn live_search_filters_extension_size_and_hidden() {
        let temp = build_sample_tree();
        let engine = LiveSearchEngine::new();

        // Hidden excluded by default
        let q_no_hidden = SearchQuery {
            text: "hidden".to_string(),
            include_hidden: false,
            ..SearchQuery::default()
        };
        let res = engine
            .search(
                temp.path().to_path_buf(),
                q_no_hidden,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(res.is_empty());

        // Hidden included when requested
        let q_hidden = SearchQuery {
            text: "hidden".to_string(),
            include_hidden: true,
            ..SearchQuery::default()
        };
        let res = engine
            .search(
                temp.path().to_path_buf(),
                q_hidden,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].entry.name, ".hidden_secret.txt");

        // Extension + min_size filter
        let q_size = SearchQuery {
            extensions: vec![".bin".to_string()],
            min_size: Some(2048),
            max_size: Some(8192),
            ..SearchQuery::default()
        };
        let res = engine
            .search(temp.path().to_path_buf(), q_size, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].entry.name, "big_file.bin");
    }

    #[tokio::test]
    async fn live_search_streaming_respects_cancellation() {
        let temp = build_sample_tree();
        let engine = LiveSearchEngine::with_batch_size(2);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let err = engine
            .search_streaming(
                temp.path().to_path_buf(),
                SearchQuery::default(),
                cancel,
                tx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, OnyxError::Cancelled));
    }

    #[test]
    fn sqlite_index_excludes_dirs_and_escapes_fts5_special_chars() {
        let temp = build_sample_tree();
        let db_dir = TempDir::new().unwrap();
        let db_path = db_dir.path().join("search.db");

        let mut index = SearchIndex::open(&db_path).unwrap();
        index.add_excluded_dir(temp.path().join("node_modules"));

        let indexed = index
            .index_directory(temp.path(), &CancellationToken::new())
            .unwrap();
        assert!(indexed >= 6);

        let stats = index.stats().unwrap();
        assert_eq!(stats.total_entries, indexed);
        assert!(stats.last_indexed_at.is_some());

        // Excluded dir entry must NOT be in the index
        let dep_hits = index
            .query(
                Some(temp.path()),
                &SearchQuery {
                    text: "alpha_dep".to_string(),
                    ..SearchQuery::default()
                },
            )
            .unwrap();
        assert!(dep_hits.is_empty());

        // Special characters (dots, hyphens, quotes, FTS5 keywords, punctuation-only) must never error
        let tricky_queries = [
            "report-2026.final.txt",
            "\"quoted\"",
            "NOT AND OR NEAR",
            "---",
            "...",
            "alpha*beta:gamma(test)",
            "report-2026",
        ];
        for q in tricky_queries {
            let res = index.query(
                Some(temp.path()),
                &SearchQuery {
                    text: q.to_string(),
                    ..SearchQuery::default()
                },
            );
            assert!(res.is_ok(), "FTS5 query failed for input {q:?}: {res:?}");
        }

        let report_hits = index
            .query(
                Some(temp.path()),
                &SearchQuery {
                    text: "report-2026.final".to_string(),
                    ..SearchQuery::default()
                },
            )
            .unwrap();
        assert_eq!(report_hits.len(), 1);
        assert_eq!(report_hits[0].entry.name, "report-2026.final.txt");
        assert_eq!(report_hits[0].source, SearchSource::SqliteIndex);
    }

    #[test]
    fn sqlite_index_upsert_remove_and_reconcile_prune_stale() {
        let temp = build_sample_tree();
        let index = SearchIndex::open_in_memory().unwrap();
        let cancel = CancellationToken::new();

        let initial = index.index_directory(temp.path(), &cancel).unwrap();
        assert!(initial >= 6);

        // Delete a file on disk and verify reconciliation prunes it
        let doomed = temp.path().join("notes.md");
        fs::remove_file(&doomed).unwrap();
        let pruned = index.reconcile_directory(temp.path()).unwrap();
        assert_eq!(pruned, 1);

        let hits = index
            .query(
                Some(temp.path()),
                &SearchQuery {
                    text: "notes".to_string(),
                    ..SearchQuery::default()
                },
            )
            .unwrap();
        assert!(hits.is_empty());

        // Upsert an entry then explicitly remove it
        let synthetic_path = temp.path().join("synthetic_item.rs");
        let entry = FileEntry {
            path: synthetic_path.clone(),
            name: "synthetic_item.rs".to_string(),
            extension: Some("rs".to_string()),
            kind: FileKind::File,
            size: 128,
            modified: Some(1_700_000_000),
            created: None,
            attributes: FileAttributes::default(),
            is_symlink: false,
            symlink_target: None,
            type_label: "Rust Source".to_string(),
        };
        index.upsert_entry(&entry).unwrap();

        let found = index
            .query(
                Some(temp.path()),
                &SearchQuery {
                    text: "synthetic".to_string(),
                    ..SearchQuery::default()
                },
            )
            .unwrap();
        assert_eq!(found.len(), 1);

        index.remove_path(&synthetic_path).unwrap();
        let after_remove = index
            .query(
                Some(temp.path()),
                &SearchQuery {
                    text: "synthetic".to_string(),
                    ..SearchQuery::default()
                },
            )
            .unwrap();
        assert!(after_remove.is_empty());
    }

    #[test]
    fn sqlite_index_respects_cancellation() {
        let temp = build_sample_tree();
        let index = SearchIndex::open_in_memory().unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let err = index.index_directory(temp.path(), &cancel).unwrap_err();
        assert!(matches!(err, OnyxError::Cancelled));
    }

    #[tokio::test]
    async fn unified_engine_uses_index_and_falls_back_gracefully() {
        let indexed_dir = build_sample_tree();
        let unindexed_dir = build_sample_tree();

        let index = SearchIndex::open_in_memory().unwrap();
        index
            .index_directory(indexed_dir.path(), &CancellationToken::new())
            .unwrap();
        let shared_index = Arc::new(Mutex::new(index));
        let engine = UnifiedSearchEngine::with_index(shared_index);

        // Indexed root with use_index=true -> SqliteIndex
        let indexed_hits = engine
            .search(
                indexed_dir.path().to_path_buf(),
                SearchQuery {
                    text: "alpha".to_string(),
                    use_index: true,
                    ..SearchQuery::default()
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!indexed_hits.is_empty());
        assert!(
            indexed_hits
                .iter()
                .all(|r| r.source == SearchSource::SqliteIndex)
        );

        // Indexed root with use_index=false -> LiveWalk
        let forced_live = engine
            .search(
                indexed_dir.path().to_path_buf(),
                SearchQuery {
                    text: "alpha".to_string(),
                    use_index: false,
                    ..SearchQuery::default()
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!forced_live.is_empty());
        assert!(
            forced_live
                .iter()
                .all(|r| r.source == SearchSource::LiveWalk)
        );

        // Unindexed root with use_index=true -> falls back to LiveWalk
        let fallback_hits = engine
            .search(
                unindexed_dir.path().to_path_buf(),
                SearchQuery {
                    text: "alpha".to_string(),
                    use_index: true,
                    ..SearchQuery::default()
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!fallback_hits.is_empty());
        assert!(
            fallback_hits
                .iter()
                .all(|r| r.source == SearchSource::LiveWalk)
        );

        // Non-existent root returns NotFound
        let missing = engine
            .search(
                unindexed_dir.path().join("does_not_exist"),
                SearchQuery::default(),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(missing, Err(OnyxError::NotFound { .. })));

        // Streaming via UnifiedSearchEngine over indexed root
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let streamed_count = engine
            .search_streaming(
                indexed_dir.path().to_path_buf(),
                SearchQuery {
                    text: "alpha beta".to_string(),
                    use_index: true,
                    ..SearchQuery::default()
                },
                CancellationToken::new(),
                tx,
            )
            .await
            .unwrap();
        assert_eq!(streamed_count, 1);
        let batch = rx.recv().await.unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].entry.name, "alpha_beta.rs");
        assert_eq!(batch[0].source, SearchSource::SqliteIndex);
    }

    #[test]
    fn sqlite_index_filters_by_modified_timestamp_range() {
        let index = SearchIndex::open_in_memory().unwrap();
        let make_entry = |name: &str, modified: i64| FileEntry {
            path: PathBuf::from(format!(r"C:\test_ts\{name}")),
            name: name.to_string(),
            extension: Some("txt".to_string()),
            kind: FileKind::File,
            size: 10,
            modified: Some(modified),
            created: None,
            attributes: FileAttributes::default(),
            is_symlink: false,
            symlink_target: None,
            type_label: "Text Document".to_string(),
        };

        index.upsert_entry(&make_entry("old.txt", 1_000)).unwrap();
        index.upsert_entry(&make_entry("mid.txt", 2_000)).unwrap();
        index.upsert_entry(&make_entry("new.txt", 3_000)).unwrap();

        let hits = index
            .query(
                None,
                &SearchQuery {
                    modified_after: Some(1_500),
                    modified_before: Some(2_500),
                    ..SearchQuery::default()
                },
            )
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.name, "mid.txt");
    }
}
