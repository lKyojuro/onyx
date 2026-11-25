//! End-to-end integration tests for Onyx engines.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use image::{ImageBuffer, Rgba};
use onyx_cache::{DirMetadataRecord, DirectoryCache, MetadataStore};
use onyx_config::{AppConfig, ConfigStore, SessionState};
use onyx_core::{
    FavoriteItem, FileKind, GenerationId, KnownFolderKind, NavHistory, OnyxError, OnyxResult,
    TabManager, ThemeMode, ViewMode, build_breadcrumbs,
};
use onyx_fs::{EnumerationOptions, FsEngine};
use onyx_operations::{
    ConflictPolicy, FileOperationsEngine, PermanentDeleteRequest, Recycler,
    generate_auto_rename_path,
};
use onyx_preview::{PreviewContent, PreviewEngine, PreviewLimits};
use onyx_search::{LiveSearchEngine, SearchIndex, SearchQuery, SearchSource, UnifiedSearchEngine};
use onyx_windows::{
    UsnCapability, detect_system_theme, enumerate_drives, enumerate_known_folders,
    probe_ntfs_usn_capability,
};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// Isolated mock [`Recycler`] that moves recycled paths into a temporary trash directory
/// inside a `TempDir`, ensuring tests never touch the user's real Windows Recycle Bin.
#[derive(Debug)]
struct MockRecycler {
    trash_dir: PathBuf,
    recycled: Mutex<Vec<PathBuf>>,
}

impl MockRecycler {
    fn new(trash_dir: PathBuf) -> Self {
        Self {
            trash_dir,
            recycled: Mutex::new(Vec::new()),
        }
    }

    fn recycled_paths(&self) -> Vec<PathBuf> {
        self.recycled.lock().unwrap().clone()
    }
}

impl Recycler for MockRecycler {
    fn recycle(&self, paths: &[PathBuf]) -> OnyxResult<()> {
        for path in paths {
            let name = path.file_name().ok_or_else(|| OnyxError::InvalidPath {
                message: format!("path has no filename: {}", path.display()),
            })?;
            let mut dest = self.trash_dir.join(name);
            if dest.exists() {
                dest = generate_auto_rename_path(&dest)?;
            }
            fs::rename(path, &dest)?;
            self.recycled.lock().unwrap().push(path.clone());
        }
        Ok(())
    }
}

#[tokio::test]
async fn e2e_navigation_generation_tracking_and_cache() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    let sub_b = root.join("folder_b");
    let sub_a = root.join("folder_a");
    fs::create_dir(&sub_b).unwrap();
    fs::create_dir(&sub_a).unwrap();

    fs::write(root.join("file10.txt"), b"ten").unwrap();
    fs::write(root.join("file2.txt"), b"two").unwrap();
    fs::write(root.join("file1.txt"), b"one").unwrap();
    fs::write(sub_a.join("inside_a.rs"), b"fn a() {}").unwrap();

    let fs_engine = FsEngine::new();
    let dir_cache = DirectoryCache::new(16);
    let mut tab_mgr = TabManager::new(root.to_path_buf());

    let gen1 = tab_mgr.active_tab().unwrap().generation;
    assert_eq!(gen1, GenerationId::new(1));

    let entries = fs_engine
        .enumerate_dir(
            root.to_path_buf(),
            gen1,
            EnumerationOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    dir_cache.put(root.to_path_buf(), entries.clone());
    let cached = dir_cache.get(root).unwrap();
    assert_eq!(cached.len(), 5);

    // Folders first, then natural sort order (`file1.txt` < `file2.txt` < `file10.txt`).
    let names: Vec<&str> = cached.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "folder_a",
            "folder_b",
            "file1.txt",
            "file2.txt",
            "file10.txt"
        ]
    );
    assert_eq!(cached[0].kind, FileKind::Directory);
    assert_eq!(cached[2].kind, FileKind::File);

    // Pre-cancelled token aborts cleanly with `OnyxError::Cancelled`.
    let cancelled_token = CancellationToken::new();
    cancelled_token.cancel();
    let abort_res = fs_engine
        .enumerate_dir(
            root.to_path_buf(),
            gen1.next(),
            EnumerationOptions::default(),
            cancelled_token,
        )
        .await;
    assert!(matches!(abort_res, Err(OnyxError::Cancelled)));

    // TabManager + NavHistory + build_breadcrumbs integration.
    let gen2 = tab_mgr
        .active_tab_mut()
        .unwrap()
        .navigate(sub_a.clone(), true);
    assert_eq!(gen2, GenerationId::new(2));
    assert_eq!(tab_mgr.active_tab().unwrap().path, sub_a);
    assert!(tab_mgr.active_tab().unwrap().history.can_go_back());

    let back_path = tab_mgr
        .active_tab_mut()
        .unwrap()
        .history
        .go_back()
        .cloned()
        .unwrap();
    assert_eq!(back_path, root.to_path_buf());
    assert!(tab_mgr.active_tab().unwrap().history.can_go_forward());

    let fwd_path = tab_mgr
        .active_tab_mut()
        .unwrap()
        .history
        .go_forward()
        .cloned()
        .unwrap();
    assert_eq!(fwd_path, sub_a);

    let mut standalone_history = NavHistory::with_initial(root.to_path_buf());
    standalone_history.push(sub_a.clone());
    standalone_history.push(sub_b.clone());
    assert_eq!(standalone_history.go_back(), Some(&sub_a));
    assert_eq!(standalone_history.go_forward(), Some(&sub_b));

    let tab2_idx = tab_mgr.new_tab(sub_b.clone());
    assert_eq!(tab2_idx, 1);
    assert_eq!(tab_mgr.tabs().len(), 2);
    assert!(tab_mgr.reorder_tab(1, 0));
    assert_eq!(tab_mgr.active_index(), 0);
    assert!(tab_mgr.switch_tab(1));
    assert!(tab_mgr.close_tab(1));
    assert_eq!(tab_mgr.tabs().len(), 1);

    let crumbs = build_breadcrumbs(&sub_a);
    assert!(crumbs.len() >= 2);
    assert_eq!(crumbs.last().unwrap().label, "folder_a");
    assert_eq!(crumbs.last().unwrap().path, sub_a);
}

#[tokio::test]
async fn e2e_file_operations_pipeline_and_undo_sequence() {
    let work = TempDir::new().unwrap();
    let trash = TempDir::new().unwrap();
    let recycler = Arc::new(MockRecycler::new(trash.path().to_path_buf()));
    let engine = FileOperationsEngine::with_recycler(Arc::clone(&recycler) as Arc<dyn Recycler>);

    // 1. create_dir
    let src_rep = engine.create_dir(work.path(), "src_dir").await.unwrap();
    let dst_rep = engine.create_dir(work.path(), "dst_dir").await.unwrap();
    let src_dir = src_rep.succeeded[0].clone();
    let dst_dir = dst_rep.succeeded[0].clone();
    assert!(src_dir.is_dir());
    assert!(dst_dir.is_dir());

    let origin_file = src_dir.join("note.txt");
    fs::write(&origin_file, b"v1-content").unwrap();

    // 2. copy_items: initial copy, then AutoRename, Skip, and Overwrite conflict policies
    let copy1 = engine
        .copy_items(
            std::slice::from_ref(&origin_file),
            &dst_dir,
            ConflictPolicy::Fail,
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    let dst_note = dst_dir.join("note.txt");
    assert_eq!(copy1.succeeded, vec![dst_note.clone()]);
    assert_eq!(fs::read(&dst_note).unwrap(), b"v1-content");

    fs::write(&origin_file, b"v2-content").unwrap();

    // ConflictPolicy::AutoRename -> creates `note (2).txt`
    let copy_autorename = engine
        .copy_items(
            std::slice::from_ref(&origin_file),
            &dst_dir,
            ConflictPolicy::AutoRename,
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    let dst_note_2 = dst_dir.join("note (2).txt");
    assert_eq!(copy_autorename.succeeded, vec![dst_note_2.clone()]);
    assert_eq!(fs::read(&dst_note_2).unwrap(), b"v2-content");

    // ConflictPolicy::Skip -> leaves `dst_note` untouched as `v1-content`
    let copy_skip = engine
        .copy_items(
            std::slice::from_ref(&origin_file),
            &dst_dir,
            ConflictPolicy::Skip,
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(copy_skip.skipped, vec![origin_file.clone()]);
    assert_eq!(fs::read(&dst_note).unwrap(), b"v1-content");

    // ConflictPolicy::Overwrite -> overwrites `dst_note` with `v2-content`
    let copy_overwrite = engine
        .copy_items(
            std::slice::from_ref(&origin_file),
            &dst_dir,
            ConflictPolicy::Overwrite,
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(copy_overwrite.succeeded, vec![dst_note.clone()]);
    assert_eq!(fs::read(&dst_note).unwrap(), b"v2-content");

    // 3. duplicate_item -> creates `note (2).txt` inside `src_dir`
    let dup_rep = engine
        .duplicate_item(&origin_file, CancellationToken::new(), None)
        .await
        .unwrap();
    let dup_path = src_dir.join("note (2).txt");
    assert_eq!(dup_rep.succeeded, vec![dup_path.clone()]);
    assert!(dup_path.exists());

    // 4. rename_item -> renames `note (2).txt` to `renamed_note.txt`
    let ren_rep = engine
        .rename_item(&dup_path, "renamed_note.txt", ConflictPolicy::Fail)
        .await
        .unwrap();
    let renamed_path = src_dir.join("renamed_note.txt");
    assert_eq!(ren_rep.succeeded, vec![renamed_path.clone()]);
    assert!(renamed_path.exists());
    assert!(!dup_path.exists());

    // 5. move_items -> moves `renamed_note.txt` into `dst_dir`
    let move_rep = engine
        .move_items(
            std::slice::from_ref(&renamed_path),
            &dst_dir,
            ConflictPolicy::Fail,
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    let moved_path = dst_dir.join("renamed_note.txt");
    assert_eq!(move_rep.succeeded, vec![moved_path.clone()]);
    assert!(moved_path.exists());
    assert!(!renamed_path.exists());

    // 6. Recursion-guard refusal (`OnyxError::RecursiveOperation`)
    let child_in_src = src_dir.join("nested_child");
    fs::create_dir(&child_in_src).unwrap();
    let rec_err = engine
        .copy_items(
            std::slice::from_ref(&src_dir),
            &child_in_src,
            ConflictPolicy::AutoRename,
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(rec_err, OnyxError::RecursiveOperation { .. }));

    // 7. Unconfirmed permanent delete refusal & confirmed permanent delete
    let perm_target = work.path().join("ephemeral.txt");
    fs::write(&perm_target, b"delete-me-permanently").unwrap();

    let unconfirmed_err = engine
        .permanent_delete(
            PermanentDeleteRequest::unconfirmed(vec![perm_target.clone()]),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(unconfirmed_err, OnyxError::OperationFailed { .. }));
    assert!(perm_target.exists());

    let confirmed_rep = engine
        .permanent_delete(
            PermanentDeleteRequest::confirmed(vec![perm_target.clone()]),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(confirmed_rep.succeeded, vec![perm_target.clone()]);
    assert!(!perm_target.exists());

    // 8. recycle_items via mock recycler
    let recycle_target = work.path().join("to_recycle.txt");
    fs::write(&recycle_target, b"recycle-me").unwrap();
    let recycle_rep = engine
        .recycle_items(
            std::slice::from_ref(&recycle_target),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(recycle_rep.succeeded, vec![recycle_target.clone()]);
    assert!(!recycle_target.exists());
    assert!(recycler.recycled_paths().contains(&recycle_target));

    // 9. undo_last sequence:
    // Undo move (`dst_dir/renamed_note.txt` -> `src_dir/renamed_note.txt`)
    let undo_move = engine.undo_last().await.unwrap().unwrap();
    assert_eq!(undo_move.succeeded, vec![renamed_path.clone()]);
    assert!(renamed_path.exists());
    assert!(!moved_path.exists());

    // Undo rename (`src_dir/renamed_note.txt` -> `src_dir/note (2).txt`)
    let undo_rename = engine.undo_last().await.unwrap().unwrap();
    assert_eq!(undo_rename.succeeded, vec![dup_path.clone()]);
    assert!(dup_path.exists());
    assert!(!renamed_path.exists());

    // Undo duplicate (recycles `src_dir/note (2).txt` via MockRecycler)
    let undo_dup = engine.undo_last().await.unwrap().unwrap();
    assert_eq!(undo_dup.succeeded, vec![dup_path.clone()]);
    assert!(!dup_path.exists());
    assert!(recycler.recycled_paths().contains(&dup_path));
}

#[tokio::test]
async fn e2e_dual_mode_search_pipeline() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    fs::write(root.join("engine_core.rs"), "pub fn run_engine() {}").unwrap();
    fs::write(root.join("readme.md"), "# Onyx Search").unwrap();
    fs::write(root.join("report-2026.final.txt"), vec![b'a'; 4096]).unwrap();

    let node_modules = root.join("node_modules");
    fs::create_dir(&node_modules).unwrap();
    fs::write(node_modules.join("engine_bundle.rs"), "// excluded dep").unwrap();

    // 1. LiveSearchEngine extension & size filters + cancellation
    let live = LiveSearchEngine::new();
    let ext_filtered = live
        .search(
            root.to_path_buf(),
            SearchQuery {
                text: String::new(),
                extensions: vec!["txt".to_string()],
                min_size: Some(2048),
                max_size: Some(8192),
                ..SearchQuery::default()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(ext_filtered.len(), 1);
    assert_eq!(ext_filtered[0].entry.name, "report-2026.final.txt");
    assert_eq!(ext_filtered[0].source, SearchSource::LiveWalk);

    let pre_cancelled = CancellationToken::new();
    pre_cancelled.cancel();
    let live_cancel_err = live
        .search(root.to_path_buf(), SearchQuery::default(), pre_cancelled)
        .await
        .unwrap_err();
    assert!(matches!(live_cancel_err, OnyxError::Cancelled));

    // 2. SearchIndex in-memory indexing with excluded `node_modules/`
    let mut index = SearchIndex::open_in_memory().unwrap();
    index.add_excluded_dir(PathBuf::from("node_modules"));
    let count = index
        .index_directory(root, &CancellationToken::new())
        .unwrap();
    assert_eq!(count, 3);

    // Excluded `node_modules/engine_bundle.rs` must not appear
    let rs_hits = index
        .query(
            Some(root),
            &SearchQuery {
                text: "engine".to_string(),
                ..SearchQuery::default()
            },
        )
        .unwrap();
    assert_eq!(rs_hits.len(), 1);
    assert_eq!(rs_hits[0].entry.name, "engine_core.rs");
    assert_eq!(rs_hits[0].source, SearchSource::SqliteIndex);

    // 3. FTS5 search with special characters (hyphens, dots, quotes, operators)
    for tricky in [
        "report-2026.final.txt",
        "\"quoted\"",
        "NOT AND OR NEAR",
        "---",
        "alpha*beta:gamma(test)",
    ] {
        let res = index.query(
            Some(root),
            &SearchQuery {
                text: tricky.to_string(),
                ..SearchQuery::default()
            },
        );
        assert!(res.is_ok(), "FTS5 query failed on {tricky:?}: {res:?}");
    }

    let special_hits = index
        .query(
            Some(root),
            &SearchQuery {
                text: "report-2026.final".to_string(),
                ..SearchQuery::default()
            },
        )
        .unwrap();
    assert_eq!(special_hits.len(), 1);
    assert_eq!(special_hits[0].entry.name, "report-2026.final.txt");

    // 4. Delete a file on disk, run `reconcile_directory`, and verify stale entry is removed
    fs::remove_file(root.join("readme.md")).unwrap();
    let pruned = index.reconcile_directory(root).unwrap();
    assert_eq!(pruned, 1);

    let after_prune = index
        .query(
            Some(root),
            &SearchQuery {
                text: "readme".to_string(),
                ..SearchQuery::default()
            },
        )
        .unwrap();
    assert!(after_prune.is_empty());

    // 5. UnifiedSearchEngine indexed vs unindexed fallback
    let unindexed_temp = TempDir::new().unwrap();
    fs::write(unindexed_temp.path().join("fallback_item.rs"), "fn f() {}").unwrap();

    let unified = UnifiedSearchEngine::with_index(Arc::new(Mutex::new(index)));

    let indexed_res = unified
        .search(
            root.to_path_buf(),
            SearchQuery {
                text: "engine".to_string(),
                use_index: true,
                ..SearchQuery::default()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(indexed_res.len(), 1);
    assert_eq!(indexed_res[0].source, SearchSource::SqliteIndex);

    let fallback_res = unified
        .search(
            unindexed_temp.path().to_path_buf(),
            SearchQuery {
                text: "fallback_item".to_string(),
                use_index: true,
                ..SearchQuery::default()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(fallback_res.len(), 1);
    assert_eq!(fallback_res[0].entry.name, "fallback_item.rs");
    assert_eq!(fallback_res[0].source, SearchSource::LiveWalk);
}

#[tokio::test]
async fn e2e_preview_pipeline() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    // 1. Real small PNG via `image` crate
    let png_path = root.join("icon.png");
    let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_fn(32, 16, |x, y| {
        Rgba([(x * 4) as u8, (y * 8) as u8, 200, 255])
    });
    img.save(&png_path).unwrap();

    // 2. Rust source file
    let rs_path = root.join("main.rs");
    fs::write(&rs_path, "fn main() {\n    println!(\"onyx\");\n}\n").unwrap();

    // 3. Oversized text file (> custom limit of 128 bytes)
    let big_txt_path = root.join("oversized.txt");
    fs::write(&big_txt_path, "0123456789abcdef\n".repeat(32)).unwrap();

    // 4. Minimal `%PDF-1.7` file
    let pdf_path = root.join("manual.pdf");
    let pdf_bytes = b"%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n\
        2 0 obj << /Type /Pages /Count 1 /Kids [3 0 R] >> endobj\n\
        3 0 obj << /Type /Page >> endobj\n%%EOF\n";
    fs::write(&pdf_path, pdf_bytes).unwrap();

    // 5. Subdirectory with known items
    let sub_dir = root.join("sub_folder");
    fs::create_dir(&sub_dir).unwrap();
    fs::write(sub_dir.join("child.txt"), b"hello").unwrap();

    let engine = PreviewEngine::with_limits(PreviewLimits {
        max_text_bytes: 128,
        ..PreviewLimits::default()
    });

    // Verify Image preview
    match engine
        .load_preview(png_path, CancellationToken::new())
        .await
        .unwrap()
    {
        PreviewContent::Image {
            width,
            height,
            original_width,
            original_height,
            rgba,
            format,
        } => {
            assert_eq!(width, 32);
            assert_eq!(height, 16);
            assert_eq!(original_width, 32);
            assert_eq!(original_height, 16);
            assert_eq!(format, "PNG");
            assert_eq!(rgba.len(), 32 * 16 * 4);
        }
        other => panic!("expected Image preview, got {other:?}"),
    }

    // Verify Rust Text preview
    match engine
        .load_preview(rs_path, CancellationToken::new())
        .await
        .unwrap()
    {
        PreviewContent::Text {
            text,
            language,
            line_count,
            truncated,
        } => {
            assert_eq!(language, "Rust");
            assert_eq!(line_count, 3);
            assert!(!truncated);
            assert!(text.contains("println!"));
        }
        other => panic!("expected Text preview, got {other:?}"),
    }

    // Verify truncated Text preview
    match engine
        .load_preview(big_txt_path, CancellationToken::new())
        .await
        .unwrap()
    {
        PreviewContent::Text {
            text,
            language,
            truncated,
            ..
        } => {
            assert_eq!(language, "Plain Text");
            assert!(truncated);
            assert!(text.len() <= 128);
        }
        other => panic!("expected truncated Text preview, got {other:?}"),
    }

    // Verify PDF DocumentMetadata preview
    match engine
        .load_preview(pdf_path, CancellationToken::new())
        .await
        .unwrap()
    {
        PreviewContent::DocumentMetadata {
            kind,
            summary_fields,
        } => {
            assert_eq!(kind, "PDF Document");
            assert!(
                summary_fields
                    .iter()
                    .any(|(k, v)| k == "PDF Version" && v == "1.7")
            );
        }
        other => panic!("expected PDF DocumentMetadata preview, got {other:?}"),
    }

    // Verify DirectorySummary preview
    match engine
        .load_preview(sub_dir, CancellationToken::new())
        .await
        .unwrap()
    {
        PreviewContent::DirectorySummary {
            item_count,
            dir_count,
            file_count,
            immediate_size_bytes,
            truncated,
        } => {
            assert_eq!(item_count, 1);
            assert_eq!(dir_count, 0);
            assert_eq!(file_count, 1);
            assert_eq!(immediate_size_bytes, 5);
            assert!(!truncated);
        }
        other => panic!("expected DirectorySummary preview, got {other:?}"),
    }
}

#[test]
fn e2e_config_session_and_cache_crash_recovery() {
    let temp = TempDir::new().unwrap();
    let store = ConfigStore::with_dir(temp.path().join("config_root"));

    let mut cfg = AppConfig {
        theme: ThemeMode::Light,
        accent_hex: Some("#0078D4".to_string()),
        show_hidden: true,
        show_system: false,
        default_view: ViewMode::Grid,
        dual_pane_default: true,
        show_preview_pane: true,
        default_terminal: "wt".to_string(),
        favorites: Vec::new(),
        excluded_index_dirs: vec![temp.path().join("node_modules")],
        recent_locations: Vec::new(),
    };
    cfg.add_favorite(FavoriteItem {
        name: "Work".to_string(),
        path: temp.path().to_path_buf(),
    });
    cfg.record_recent(temp.path().to_path_buf());

    store.save_config(&cfg).unwrap();
    assert_eq!(store.load_config(), cfg);

    let session = SessionState {
        tabs: vec![temp.path().to_path_buf()],
        active_tab: 0,
        dual_pane: true,
        secondary_path: Some(temp.path().to_path_buf()),
        window_width: 1600,
        window_height: 900,
        ..SessionState::default()
    };
    store.save_session(&session).unwrap();
    assert_eq!(store.load_session(), session);

    // Corrupt config and session files -> verify graceful fallback to defaults
    fs::write(store.config_path(), b"<<<corrupt_toml_payload>>>").unwrap();
    assert_eq!(store.load_config(), AppConfig::default());

    fs::write(store.session_path(), b"<<<corrupt_json_payload>>>").unwrap();
    assert_eq!(store.load_session(), SessionState::default());

    // MetadataStore visit tracking & corruption recovery
    let db_path = temp.path().join("cache").join("metadata.db");
    let folder_a = temp.path().join("folder_a");
    {
        let meta_store = MetadataStore::open(&db_path).unwrap();
        let v1 = meta_store.record_visit(&folder_a, 1_700_000_000).unwrap();
        assert_eq!(v1.visit_count, 1);
        let v2 = meta_store.record_visit(&folder_a, 1_700_000_500).unwrap();
        assert_eq!(v2.visit_count, 2);

        let dir_rec = DirMetadataRecord {
            path: folder_a.clone(),
            entry_count: 15,
            total_size: 4096,
            modified_at: Some(1_700_000_000),
            updated_at: 1_700_000_500,
        };
        meta_store.upsert_dir_metadata(&dir_rec).unwrap();
        assert_eq!(
            meta_store.get_dir_metadata(&folder_a).unwrap(),
            Some(dir_rec)
        );
    }

    // Overwrite SQLite file with corrupt bytes and verify `MetadataStore::open` recovers cleanly
    fs::write(&db_path, b"corrupted sqlite database header bytes").unwrap();
    let recovered_store = MetadataStore::open(&db_path).unwrap();
    let v_after = recovered_store
        .record_visit(&folder_a, 1_700_001_000)
        .unwrap();
    assert_eq!(v_after.visit_count, 1);
    assert_eq!(recovered_store.top_visited(5).unwrap().len(), 1);
}

#[test]
fn e2e_readonly_windows_integration_checks() {
    let drives = enumerate_drives();
    assert!(
        !drives.is_empty(),
        "enumerate_drives() must return at least one drive"
    );
    assert!(drives.iter().any(|d| d.root.is_absolute()));

    let known_folders = enumerate_known_folders();
    assert!(
        !known_folders.is_empty(),
        "enumerate_known_folders() must return a non-empty list"
    );
    assert!(
        known_folders
            .iter()
            .any(|f| f.kind == KnownFolderKind::Home)
    );

    let theme = detect_system_theme();
    assert!(matches!(
        theme,
        ThemeMode::Dark | ThemeMode::Light | ThemeMode::System
    ));

    let temp = TempDir::new().unwrap();
    let usn = probe_ntfs_usn_capability(temp.path());
    assert!(matches!(
        usn,
        UsnCapability::Supported { .. }
            | UsnCapability::RequiresElevation
            | UsnCapability::UnsupportedFilesystem(_)
            | UsnCapability::Unavailable(_)
    ));
}
