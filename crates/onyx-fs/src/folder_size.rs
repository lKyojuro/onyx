use onyx_core::{OnyxError, OnyxResult, to_extended_length_path};
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
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

/// Asynchronously computes the recursive byte size and file count of `path` on Tokio's blocking pool.
///
/// Uses an explicit iterative stack to avoid recursion stack overflow, uses `symlink_metadata` to
/// skip symlinks and Windows junctions/reparse points (preventing cycles), checks `cancel`
/// frequently, ignores permission-denied subdirectories gracefully, and caps traversal at
/// `max_entries` entries (setting `truncated = true` when capped).
pub async fn compute_folder_size(
    path: PathBuf,
    max_entries: usize,
    cancel: CancellationToken,
) -> OnyxResult<FolderSizeResult> {
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
) -> OnyxResult<FolderSizeResult> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    let extended_root = to_extended_length_path(root);
    let root_meta =
        fs::symlink_metadata(&extended_root).map_err(|err| map_root_error(err, root))?;

    let root_type = root_meta.file_type();
    if root_type.is_symlink() || is_reparse_point(&root_meta) {
        return Ok(FolderSizeResult::default());
    }
    if root_type.is_file() {
        return Ok(FolderSizeResult {
            total_bytes: root_meta.len(),
            file_count: 1,
            truncated: false,
        });
    }
    if !root_type.is_dir() {
        return Ok(FolderSizeResult::default());
    }

    if max_entries == 0 {
        return Ok(FolderSizeResult {
            total_bytes: 0,
            file_count: 0,
            truncated: true,
        });
    }

    let mut stack = vec![root.to_path_buf()];
    let mut total_bytes: u64 = 0;
    let mut file_count: u64 = 0;
    let mut entries_seen: usize = 0;
    let mut truncated = false;

    while let Some(dir_path) = stack.pop() {
        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        let extended_dir = to_extended_length_path(&dir_path);
        let read_dir = match fs::read_dir(&extended_dir) {
            Ok(rd) => rd,
            Err(err) => {
                if dir_path == root && err.kind() == std::io::ErrorKind::NotFound {
                    return Err(OnyxError::NotFound {
                        path: root.to_path_buf(),
                    });
                }
                // Gracefully ignore permission-denied or unreadable subdirectories.
                continue;
            }
        };

        for entry_res in read_dir {
            if cancel.is_cancelled() {
                return Err(OnyxError::Cancelled);
            }

            let Ok(dir_entry) = entry_res else {
                continue;
            };

            if entries_seen >= max_entries {
                truncated = true;
                break;
            }

            // On Windows, `DirEntry::metadata()` reuses `WIN32_FIND_DATA` from `FindNextFileW`
            // with zero extra syscalls and reports symlinks/reparse points without following links.
            let Ok(meta) = dir_entry.metadata() else {
                continue;
            };

            let file_type = meta.file_type();
            if file_type.is_symlink() || is_reparse_point(&meta) {
                continue;
            }

            entries_seen += 1;

            if file_type.is_dir() {
                stack.push(dir_path.join(dir_entry.file_name()));
            } else if file_type.is_file() {
                total_bytes = total_bytes.saturating_add(meta.len());
                file_count = file_count.saturating_add(1);
            }
        }

        if truncated {
            break;
        }
    }

    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    Ok(FolderSizeResult {
        total_bytes,
        file_count,
        truncated,
    })
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
}
