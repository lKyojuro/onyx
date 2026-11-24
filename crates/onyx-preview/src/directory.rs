use crate::{PreviewContent, PreviewLimits};
use onyx_core::{OnyxError, OnyxResult, to_extended_length_path};
use std::path::Path;
use tokio_util::sync::CancellationToken;

const CANCEL_CHECK_INTERVAL: usize = 64;

/// Produces a bounded immediate summary of a directory without recursing.
pub(crate) fn load_directory_summary(
    path: &Path,
    limits: &PreviewLimits,
    cancel: &CancellationToken,
) -> OnyxResult<PreviewContent> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    let ext_path = to_extended_length_path(path);
    let read_dir = match std::fs::read_dir(&ext_path) {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(OnyxError::PermissionDenied {
                path: path.to_path_buf(),
            });
        }
        Err(err) => return Err(OnyxError::Io(err)),
    };

    let mut item_count = 0usize;
    let mut dir_count = 0usize;
    let mut file_count = 0usize;
    let mut immediate_size_bytes = 0u64;
    let mut truncated = false;

    for entry_res in read_dir {
        if item_count.is_multiple_of(CANCEL_CHECK_INTERVAL) && cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        if item_count >= limits.max_dir_scan {
            truncated = true;
            break;
        }

        let Ok(entry) = entry_res else {
            item_count += 1;
            continue;
        };

        let entry_path = path.join(entry.file_name());
        let entry_ext = to_extended_length_path(&entry_path);
        match std::fs::symlink_metadata(&entry_ext) {
            Ok(meta) => {
                item_count += 1;
                if meta.is_dir() {
                    dir_count += 1;
                } else {
                    file_count += 1;
                    immediate_size_bytes = immediate_size_bytes.saturating_add(meta.len());
                }
            }
            Err(_) => {
                item_count += 1;
            }
        }
    }

    Ok(PreviewContent::DirectorySummary {
        item_count,
        dir_count,
        file_count,
        immediate_size_bytes,
        truncated,
    })
}
