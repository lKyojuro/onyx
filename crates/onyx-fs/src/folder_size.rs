use onyx_core::{OnyxError, OnyxResult, to_extended_length_path};
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio_util::sync::CancellationToken;

/// Default maximum number of filesystem entries traversed when computing a folder's size.
pub const DEFAULT_MAX_FOLDER_SIZE_ENTRIES: usize = 100_000;

#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Result of an asynchronous recursive folder size calculation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FolderSizeResult {
    /// Total size in bytes of all regular files encountered.
    pub total_bytes: u64,
    /// Total number of regular files encountered.
    pub file_count: u64,
    /// `true` if traversal stopped early because `max_entries` was reached.
    pub truncated: bool,
}

/// Recursive folder size details for a directory subtree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtreeFolderSize {
    /// Absolute or relative path to the directory.
    pub path: PathBuf,
    /// Total recursive size in bytes of all regular files in this subtree.
    pub total_bytes: u64,
    /// Total recursive number of regular files in this subtree.
    pub file_count: u64,
    /// `true` if this subtree was truncated due to hitting `max_entries`.
    pub truncated: bool,
    /// Directory modification time in Unix seconds (UTC), if available.
    pub mtime_secs: Option<i64>,
}

/// Comprehensive scan output containing root totals and all bottom-up subtree sizes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FolderSizeScanOutput {
    /// Summary totals for the scanned root directory.
    pub root: FolderSizeResult,
    /// Bottom-up aggregated sizes for every visited directory in the tree (including root).
    pub subtrees: Vec<SubtreeFolderSize>,
}

#[derive(Debug)]
struct DirNode {
    path: PathBuf,
    parent: Option<usize>,
    total_bytes: u64,
    file_count: u64,
    truncated: bool,
    mtime_secs: Option<i64>,
}

#[derive(Debug)]
struct SubtreeArenaResult {
    total_bytes: u64,
    file_count: u64,
    truncated: bool,
    subtrees: Vec<SubtreeFolderSize>,
}

#[cfg(not(windows))]
#[derive(Debug)]
struct FallbackDirEntry {
    name: std::ffi::OsString,
    size: u64,
    is_dir: bool,
    is_reparse_point: bool,
    mtime_secs: Option<i64>,
}

/// Asynchronously computes the recursive byte size and file count of `path` on Tokio's blocking pool.
///
/// Uses low-priority I/O, fast directory enumeration, and cycle protection.
pub async fn compute_folder_size(
    path: PathBuf,
    max_entries: usize,
    cancel: CancellationToken,
) -> OnyxResult<FolderSizeResult> {
    compute_folder_size_with_subtrees(path, max_entries, cancel)
        .await
        .map(|out| out.root)
}

/// Asynchronously computes the recursive byte size, file count, and bottom-up subtree rollup
/// of `path` on Tokio's blocking pool.
pub async fn compute_folder_size_with_subtrees(
    path: PathBuf,
    max_entries: usize,
    cancel: CancellationToken,
) -> OnyxResult<FolderSizeScanOutput> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    tokio::task::spawn_blocking(move || compute_folder_size_blocking(&path, max_entries, &cancel))
        .await
        .map_err(|err| OnyxError::OperationFailed {
            message: format!("folder size worker join error: {err}"),
        })?
}

fn compute_folder_size_blocking(
    root: &Path,
    max_entries: usize,
    cancel: &CancellationToken,
) -> OnyxResult<FolderSizeScanOutput> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    let extended_root = to_extended_length_path(root);
    let root_meta =
        fs::symlink_metadata(&extended_root).map_err(|err| map_root_error(err, root))?;

    let root_type = root_meta.file_type();
    if root_type.is_symlink() || is_reparse_point(&root_meta) {
        return Ok(FolderSizeScanOutput::default());
    }
    if root_type.is_file() {
        return Ok(FolderSizeScanOutput {
            root: FolderSizeResult {
                total_bytes: root_meta.len(),
                file_count: 1,
                truncated: false,
            },
            subtrees: Vec::new(),
        });
    }
    if !root_type.is_dir() {
        return Ok(FolderSizeScanOutput::default());
    }

    if max_entries == 0 {
        return Ok(FolderSizeScanOutput {
            root: FolderSizeResult {
                total_bytes: 0,
                file_count: 0,
                truncated: true,
            },
            subtrees: Vec::new(),
        });
    }

    #[cfg(windows)]
    let _bg_guard = onyx_windows::BackgroundPriorityGuard::enter();

    #[cfg(windows)]
    let workers = onyx_windows::recommend_scan_workers(root);
    #[cfg(not(windows))]
    let workers = 1usize;

    let root_mtime_secs = root_meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_secs()).ok());

    let entries_seen = AtomicUsize::new(0);
    let truncated = AtomicBool::new(false);

    #[cfg(windows)]
    let root_entries = onyx_windows::scan_dir_entries(root);
    #[cfg(not(windows))]
    let root_entries = scan_dir_entries_fallback(root);

    let root_entries = match root_entries {
        Ok(entries) => entries,
        Err(err @ (OnyxError::NotFound { .. } | OnyxError::PermissionDenied { .. })) => {
            return Err(err);
        }
        Err(_) => {
            return Ok(FolderSizeScanOutput::default());
        }
    };

    let mut direct_root_bytes: u64 = 0;
    let mut direct_root_files: u64 = 0;
    let mut top_subdirs: Vec<(PathBuf, Option<i64>)> = Vec::new();

    for entry in root_entries {
        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        if entry.is_reparse_point {
            continue;
        }

        if !try_reserve_entry(&entries_seen, max_entries) {
            truncated.store(true, Ordering::Relaxed);
            break;
        }

        if entry.is_dir {
            top_subdirs.push((root.join(&entry.name), entry.mtime_secs));
        } else {
            direct_root_bytes = direct_root_bytes.saturating_add(entry.size);
            direct_root_files = direct_root_files.saturating_add(1);
        }
    }

    let is_truncated = truncated.load(Ordering::Relaxed);
    let mut all_subtrees = Vec::new();
    let mut subdirs_bytes: u64 = 0;
    let mut subdirs_files: u64 = 0;
    let mut subdirs_truncated = is_truncated;

    if workers > 1 && top_subdirs.len() >= 2 && !is_truncated {
        let num_threads = workers.min(top_subdirs.len());
        let next_task = AtomicUsize::new(0);

        let worker_results: OnyxResult<Vec<SubtreeArenaResult>> = std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(num_threads);
            for _ in 0..num_threads {
                handles.push(s.spawn(|| -> OnyxResult<SubtreeArenaResult> {
                    #[cfg(windows)]
                    let _thread_bg = onyx_windows::BackgroundPriorityGuard::enter();

                    let mut total_bytes: u64 = 0;
                    let mut file_count: u64 = 0;
                    let mut thread_truncated = false;
                    let mut subtrees = Vec::new();

                    loop {
                        if cancel.is_cancelled() {
                            return Err(OnyxError::Cancelled);
                        }
                        let task_idx = next_task.fetch_add(1, Ordering::Relaxed);
                        if task_idx >= top_subdirs.len() {
                            break;
                        }

                        let (sub_path, sub_mtime) = &top_subdirs[task_idx];
                        let res = scan_subtree_arena(
                            sub_path,
                            *sub_mtime,
                            &entries_seen,
                            max_entries,
                            &truncated,
                            cancel,
                        )?;

                        total_bytes = total_bytes.saturating_add(res.total_bytes);
                        file_count = file_count.saturating_add(res.file_count);
                        if res.truncated {
                            thread_truncated = true;
                        }
                        subtrees.extend(res.subtrees);
                    }

                    Ok(SubtreeArenaResult {
                        total_bytes,
                        file_count,
                        truncated: thread_truncated,
                        subtrees,
                    })
                }));
            }

            let mut collected = Vec::with_capacity(num_threads);
            for handle in handles {
                match handle.join() {
                    Ok(res) => collected.push(res?),
                    Err(_) => {
                        return Err(OnyxError::OperationFailed {
                            message: "folder size scan worker thread panicked".to_string(),
                        });
                    }
                }
            }
            Ok(collected)
        });

        let collected = worker_results?;
        for res in collected {
            subdirs_bytes = subdirs_bytes.saturating_add(res.total_bytes);
            subdirs_files = subdirs_files.saturating_add(res.file_count);
            if res.truncated {
                subdirs_truncated = true;
            }
            all_subtrees.extend(res.subtrees);
        }
    } else {
        for (sub_path, sub_mtime) in &top_subdirs {
            let res = scan_subtree_arena(
                sub_path,
                *sub_mtime,
                &entries_seen,
                max_entries,
                &truncated,
                cancel,
            )?;
            subdirs_bytes = subdirs_bytes.saturating_add(res.total_bytes);
            subdirs_files = subdirs_files.saturating_add(res.file_count);
            if res.truncated {
                subdirs_truncated = true;
            }
            all_subtrees.extend(res.subtrees);
        }
    }

    let final_total_bytes = direct_root_bytes.saturating_add(subdirs_bytes);
    let final_file_count = direct_root_files.saturating_add(subdirs_files);
    let final_truncated = subdirs_truncated || truncated.load(Ordering::Relaxed);

    all_subtrees.push(SubtreeFolderSize {
        path: root.to_path_buf(),
        total_bytes: final_total_bytes,
        file_count: final_file_count,
        truncated: final_truncated,
        mtime_secs: root_mtime_secs,
    });

    Ok(FolderSizeScanOutput {
        root: FolderSizeResult {
            total_bytes: final_total_bytes,
            file_count: final_file_count,
            truncated: final_truncated,
        },
        subtrees: all_subtrees,
    })
}

fn scan_subtree_arena(
    sub_root: &Path,
    sub_root_mtime: Option<i64>,
    entries_seen: &AtomicUsize,
    max_entries: usize,
    global_truncated: &AtomicBool,
    cancel: &CancellationToken,
) -> OnyxResult<SubtreeArenaResult> {
    let mut nodes: Vec<DirNode> = vec![DirNode {
        path: sub_root.to_path_buf(),
        parent: None,
        total_bytes: 0,
        file_count: 0,
        truncated: false,
        mtime_secs: sub_root_mtime,
    }];
    let mut stack: Vec<usize> = vec![0];

    while let Some(current_idx) = stack.pop() {
        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        if global_truncated.load(Ordering::Relaxed) {
            nodes[current_idx].truncated = true;
            continue;
        }

        let dir_path = nodes[current_idx].path.clone();

        #[cfg(windows)]
        let entries_res = onyx_windows::scan_dir_entries(&dir_path);
        #[cfg(not(windows))]
        let entries_res = scan_dir_entries_fallback(&dir_path);

        let entries = match entries_res {
            Ok(e) => e,
            Err(_) => {
                // Gracefully ignore permission-denied or unreadable subdirectories.
                continue;
            }
        };

        for entry in entries {
            if cancel.is_cancelled() {
                return Err(OnyxError::Cancelled);
            }

            if entry.is_reparse_point {
                continue;
            }

            if !try_reserve_entry(entries_seen, max_entries) {
                global_truncated.store(true, Ordering::Relaxed);
                nodes[current_idx].truncated = true;
                break;
            }

            if entry.is_dir {
                let child_idx = nodes.len();
                nodes.push(DirNode {
                    path: dir_path.join(&entry.name),
                    parent: Some(current_idx),
                    total_bytes: 0,
                    file_count: 0,
                    truncated: false,
                    mtime_secs: entry.mtime_secs,
                });
                stack.push(child_idx);
            } else {
                nodes[current_idx].total_bytes =
                    nodes[current_idx].total_bytes.saturating_add(entry.size);
                nodes[current_idx].file_count = nodes[current_idx].file_count.saturating_add(1);
            }
        }
    }

    // Roll up child totals bottom-up into parent nodes.
    for idx in (1..nodes.len()).rev() {
        if let Some(parent_idx) = nodes[idx].parent {
            let child_bytes = nodes[idx].total_bytes;
            let child_files = nodes[idx].file_count;
            let child_truncated = nodes[idx].truncated;

            nodes[parent_idx].total_bytes =
                nodes[parent_idx].total_bytes.saturating_add(child_bytes);
            nodes[parent_idx].file_count = nodes[parent_idx].file_count.saturating_add(child_files);
            if child_truncated {
                nodes[parent_idx].truncated = true;
            }
        }
    }

    let root_node = &nodes[0];
    let total_bytes = root_node.total_bytes;
    let file_count = root_node.file_count;
    let is_truncated = root_node.truncated;

    let subtrees: Vec<SubtreeFolderSize> = nodes
        .into_iter()
        .map(|n| SubtreeFolderSize {
            path: n.path,
            total_bytes: n.total_bytes,
            file_count: n.file_count,
            truncated: n.truncated,
            mtime_secs: n.mtime_secs,
        })
        .collect();

    Ok(SubtreeArenaResult {
        total_bytes,
        file_count,
        truncated: is_truncated,
        subtrees,
    })
}

fn try_reserve_entry(entries_seen: &AtomicUsize, max_entries: usize) -> bool {
    let mut current = entries_seen.load(Ordering::Relaxed);
    loop {
        if current >= max_entries {
            return false;
        }
        match entries_seen.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

#[cfg(not(windows))]
fn scan_dir_entries_fallback(dir: &Path) -> OnyxResult<Vec<FallbackDirEntry>> {
    let mut entries = Vec::new();
    let rd = fs::read_dir(dir).map_err(|err| map_root_error(err, dir))?;
    for entry_res in rd {
        let Ok(entry) = entry_res else { continue };
        let Ok(meta) = entry.metadata() else { continue };
        let ft = meta.file_type();
        let is_dir = ft.is_dir();
        let is_reparse_point = ft.is_symlink();
        let size = meta.len();
        let mtime_secs = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_secs()).ok());
        entries.push(FallbackDirEntry {
            name: entry.file_name(),
            size,
            is_dir,
            is_reparse_point,
            mtime_secs,
        });
    }
    Ok(entries)
}

fn is_reparse_point(meta: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        false
    }
}

fn map_root_error(err: std::io::Error, path: &Path) -> OnyxError {
    match err.kind() {
        std::io::ErrorKind::NotFound => OnyxError::NotFound {
            path: path.to_path_buf(),
        },
        std::io::ErrorKind::PermissionDenied => OnyxError::PermissionDenied {
            path: path.to_path_buf(),
        },
        _ => OnyxError::Io(err),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn compute_folder_size_sums_nested_directories() {
        let tmp = TempDir::new().unwrap();
        let sub_a = tmp.path().join("a");
        let sub_b = sub_a.join("b");
        fs::create_dir_all(&sub_b).unwrap();

        fs::write(tmp.path().join("root.bin"), vec![1u8; 100]).unwrap();
        fs::write(sub_a.join("mid.bin"), vec![2u8; 250]).unwrap();
        fs::write(sub_b.join("deep1.bin"), vec![3u8; 400]).unwrap();
        fs::write(sub_b.join("deep2.bin"), vec![4u8; 50]).unwrap();

        let res = compute_folder_size(
            tmp.path().to_path_buf(),
            DEFAULT_MAX_FOLDER_SIZE_ENTRIES,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(res.total_bytes, 800);
        assert_eq!(res.file_count, 4);
        assert!(!res.truncated);
    }

    #[tokio::test]
    async fn compute_folder_size_respects_cancellation() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), b"hello").unwrap();

        let cancel = CancellationToken::new();
        cancel.cancel();

        let res = compute_folder_size(
            tmp.path().to_path_buf(),
            DEFAULT_MAX_FOLDER_SIZE_ENTRIES,
            cancel,
        )
        .await;

        assert!(matches!(res, Err(OnyxError::Cancelled)));
    }

    #[tokio::test]
    async fn compute_folder_size_caps_at_max_entries_and_sets_truncated() {
        let tmp = TempDir::new().unwrap();
        for i in 0..10 {
            fs::write(tmp.path().join(format!("f_{i}.dat")), vec![7u8; 10]).unwrap();
        }

        let res = compute_folder_size(tmp.path().to_path_buf(), 4, CancellationToken::new())
            .await
            .unwrap();

        assert!(res.truncated);
        assert_eq!(res.file_count, 4);
        assert_eq!(res.total_bytes, 40);
    }

    #[tokio::test]
    async fn compute_folder_size_with_subtrees_returns_bottom_up_hierarchy() {
        let tmp = TempDir::new().unwrap();
        let sub_a = tmp.path().join("alpha");
        let sub_b = tmp.path().join("beta");
        let sub_a_nested = sub_a.join("nested");
        fs::create_dir_all(&sub_a_nested).unwrap();
        fs::create_dir(&sub_b).unwrap();

        fs::write(tmp.path().join("root.txt"), vec![0u8; 10]).unwrap();
        fs::write(sub_a.join("a.txt"), vec![1u8; 20]).unwrap();
        fs::write(sub_a_nested.join("nested.txt"), vec![2u8; 30]).unwrap();
        fs::write(sub_b.join("b.txt"), vec![3u8; 40]).unwrap();

        let out = compute_folder_size_with_subtrees(
            tmp.path().to_path_buf(),
            DEFAULT_MAX_FOLDER_SIZE_ENTRIES,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(out.root.total_bytes, 100);
        assert_eq!(out.root.file_count, 4);
        assert!(!out.root.truncated);

        // Find individual subtrees
        let nested_st = out
            .subtrees
            .iter()
            .find(|s| s.path == sub_a_nested)
            .unwrap();
        assert_eq!(nested_st.total_bytes, 30);
        assert_eq!(nested_st.file_count, 1);
        assert!(!nested_st.truncated);

        let alpha_st = out.subtrees.iter().find(|s| s.path == sub_a).unwrap();
        assert_eq!(alpha_st.total_bytes, 50);
        assert_eq!(alpha_st.file_count, 2);
        assert!(!alpha_st.truncated);

        let beta_st = out.subtrees.iter().find(|s| s.path == sub_b).unwrap();
        assert_eq!(beta_st.total_bytes, 40);
        assert_eq!(beta_st.file_count, 1);
        assert!(!beta_st.truncated);

        let root_st = out.subtrees.iter().find(|s| s.path == tmp.path()).unwrap();
        assert_eq!(root_st.total_bytes, 100);
        assert_eq!(root_st.file_count, 4);
        assert!(!root_st.truncated);
    }

    #[tokio::test]
    async fn compute_folder_size_with_subtrees_respects_max_entries_truncation() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("sub");
        fs::create_dir(&sub).unwrap();

        for i in 0..10 {
            fs::write(sub.join(format!("item_{i}.bin")), vec![1u8; 5]).unwrap();
        }

        let out = compute_folder_size_with_subtrees(
            tmp.path().to_path_buf(),
            5,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(out.root.truncated);
        assert!(out.root.file_count <= 5);
        assert!(out.subtrees.iter().any(|s| s.truncated));
    }

    #[tokio::test]
    async fn compute_folder_size_with_subtrees_handles_reparse_or_empty() {
        let tmp = TempDir::new().unwrap();
        let empty_sub = tmp.path().join("empty");
        fs::create_dir(&empty_sub).unwrap();

        let out = compute_folder_size_with_subtrees(
            empty_sub.clone(),
            DEFAULT_MAX_FOLDER_SIZE_ENTRIES,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(out.root.total_bytes, 0);
        assert_eq!(out.root.file_count, 0);
        assert!(!out.root.truncated);
        assert_eq!(out.subtrees.len(), 1);
        assert_eq!(out.subtrees[0].path, empty_sub);
    }
}
