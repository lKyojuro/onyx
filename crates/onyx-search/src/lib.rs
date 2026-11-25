//! Dual-mode streaming and SQLite FTS5 search engine for Onyx.

pub mod index;
pub mod live;
pub mod types;
pub mod unified;
pub mod updater;

pub use index::SearchIndex;
pub use live::LiveSearchEngine;
pub use types::{DEFAULT_MAX_RESULTS, IndexStats, SearchQuery, SearchResult, SearchSource};
pub use unified::UnifiedSearchEngine;
pub use updater::{DEFAULT_INDEXER_DEBOUNCE, IncrementalIndexer};

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

    #[test]
    fn sqlite_index_fts5_special_chars_and_prefix_matching() {
        let temp = build_sample_tree();
        fs::write(temp.path().join("parens (2026) [draft].txt"), "draft").unwrap();
        let index = SearchIndex::open_in_memory().unwrap();
        index
            .index_directory(temp.path(), &CancellationToken::new())
            .unwrap();

        // Prefix wildcard and quoted queries must return expected matches without syntax errors
        for q in ["alph*", "\"alpha\"", "alpha*"] {
            let hits = index
                .query(
                    Some(temp.path()),
                    &SearchQuery {
                        text: q.to_string(),
                        ..SearchQuery::default()
                    },
                )
                .unwrap();
            assert!(
                hits.iter().any(|r| r.entry.name == "alpha.rs"),
                "Expected alpha.rs in results for query {q:?}, got {hits:?}"
            );
        }

        // Parentheses, brackets, dashes, dots, colons
        let paren_hits = index
            .query(
                Some(temp.path()),
                &SearchQuery {
                    text: "parens (2026)".to_string(),
                    ..SearchQuery::default()
                },
            )
            .unwrap();
        assert_eq!(paren_hits.len(), 1);
        assert_eq!(paren_hits[0].entry.name, "parens (2026) [draft].txt");
    }

    #[test]
    fn sqlite_index_upsert_path_remove_path_and_reconcile_respect_exclusions_and_cancel() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let sub = root.join("pkg");
        fs::create_dir_all(&sub).unwrap();
        let f1 = sub.join("lib.rs");
        let f2 = sub.join("helper.rs");
        fs::write(&f1, "pub fn lib() {}").unwrap();
        fs::write(&f2, "pub fn helper() {}").unwrap();

        let mut index = SearchIndex::open_in_memory().unwrap();

        // upsert_path on a directory indexes both the directory and its descendants
        index.upsert_path(&sub).unwrap();
        let hits = index
            .query(
                Some(root),
                &SearchQuery {
                    text: "helper".to_string(),
                    ..SearchQuery::default()
                },
            )
            .unwrap();
        assert_eq!(hits.len(), 1);

        // remove_path on a directory removes the directory and all descendants
        index.remove_path(&sub).unwrap();
        let after_dir_remove = index
            .query(
                Some(root),
                &SearchQuery {
                    text: "helper".to_string(),
                    ..SearchQuery::default()
                },
            )
            .unwrap();
        assert!(after_dir_remove.is_empty());

        // Re-index root, then add "pkg" to excluded_dirs and verify reconcile_directory prunes it
        index
            .index_directory(root, &CancellationToken::new())
            .unwrap();
        index.add_excluded_dir(PathBuf::from("pkg"));
        let pruned = index.reconcile_directory(root).unwrap();
        assert!(pruned >= 3);

        // Pre-cancelled token on reconcile_directory_with_cancel returns Cancelled
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = index
            .reconcile_directory_with_cancel(root, &cancel)
            .unwrap_err();
        assert!(matches!(err, OnyxError::Cancelled));
    }

    #[test]
    fn incremental_indexer_detects_create_rename_delete_and_concurrent_queries() {
        use std::time::{Duration, Instant};

        let temp = TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let excluded = root.join("ignored_dir");
        fs::create_dir_all(&excluded).unwrap();

        let index = SearchIndex::open_in_memory().unwrap();
        index
            .index_directory(&root, &CancellationToken::new())
            .unwrap();
        let shared = Arc::new(Mutex::new(index));

        let indexer = IncrementalIndexer::start_with_debounce(
            Arc::clone(&shared),
            vec![PathBuf::from("ignored_dir")],
            Duration::from_millis(30),
        )
        .unwrap();
        indexer.watch_root(root.clone()).unwrap();
        assert_eq!(indexer.watched_roots().len(), 1);

        let wait_for_hit = |query_text: &str, expected_count: usize| -> bool {
            let deadline = Instant::now() + Duration::from_secs(4);
            while Instant::now() < deadline {
                if let Ok(guard) = shared.lock() {
                    let res = guard
                        .query(
                            Some(&root),
                            &SearchQuery {
                                text: query_text.to_string(),
                                ..SearchQuery::default()
                            },
                        )
                        .unwrap();
                    if res.len() == expected_count {
                        return true;
                    }
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            false
        };

        // 1. Create a file -> detected and indexed automatically
        let created = root.join("live_created.txt");
        fs::write(&created, "hello indexer").unwrap();
        // Also write inside excluded dir -> must never be indexed
        fs::write(excluded.join("live_created_ignored.txt"), "ignored").unwrap();

        assert!(
            wait_for_hit("live_created.txt", 1),
            "IncrementalIndexer did not index newly created file"
        );
        assert!(
            wait_for_hit("live_created_ignored", 0),
            "Excluded directory file must not be indexed"
        );

        // 2. Rename the file -> old name removed, new name indexed
        let renamed = root.join("live_renamed.txt");
        fs::rename(&created, &renamed).unwrap();
        assert!(
            wait_for_hit("live_renamed.txt", 1),
            "IncrementalIndexer did not index renamed file"
        );
        assert!(
            wait_for_hit("live_created.txt", 0),
            "IncrementalIndexer did not prune old renamed path"
        );

        // 3. Concurrent reader thread querying while files are created and deleted
        let reader_shared = Arc::clone(&shared);
        let reader_root = root.clone();
        let reader_handle = std::thread::spawn(move || {
            for _ in 0..40 {
                let guard = reader_shared.lock().unwrap();
                let _ = guard
                    .query(
                        Some(&reader_root),
                        &SearchQuery {
                            text: "live*".to_string(),
                            ..SearchQuery::default()
                        },
                    )
                    .unwrap();
                drop(guard);
                std::thread::sleep(Duration::from_millis(5));
            }
        });

        // 4. Delete the file -> pruned automatically
        fs::remove_file(&renamed).unwrap();
        assert!(
            wait_for_hit("live_renamed.txt", 0),
            "IncrementalIndexer did not prune deleted file"
        );

        reader_handle.join().unwrap();
        indexer.unwatch_root(&root);
        assert!(indexer.watched_roots().is_empty());
        indexer.shutdown();
    }
}
