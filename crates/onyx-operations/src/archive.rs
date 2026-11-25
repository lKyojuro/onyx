//! Safe ZIP archive listing, extraction (with strict Zip-Slip and zip-bomb guards),
//! and atomic archive creation off the UI thread.

use crate::path_guard::{
    generate_auto_rename_path, is_dir_reparse, is_reparse_or_symlink, is_same_normalized_path,
    is_same_or_descendant, path_exists_any, validate_item_name,
};
use crate::types::{ConflictPolicy, OperationKind, OperationProgress, OperationReport};
use crate::undo::UndoAction;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use onyx_core::{OnyxError, OnyxResult};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const ARCHIVE_CHUNK_BYTES: usize = 256 * 1024;
static NEXT_ARCHIVE_OP_ID: AtomicU64 = AtomicU64::new(100_000);

/// Default maximum number of entries permitted in a single archive (50,000).
pub const DEFAULT_MAX_ARCHIVE_ENTRIES: usize = 50_000;

/// Default maximum total uncompressed bytes permitted when extracting an archive (4 GiB).
pub const DEFAULT_MAX_ARCHIVE_UNCOMPRESSED_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Default maximum uncompressed-to-compressed ratio allowed before triggering zip-bomb rejection.
pub const DEFAULT_MAX_COMPRESSION_RATIO: u64 = 200;

/// Minimum uncompressed byte threshold (1 MiB) before applying the compression ratio guard,
/// preventing false positives on small highly repetitive text files.
pub const RATIO_GUARD_MIN_UNCOMPRESSED_BYTES: u64 = 1024 * 1024;

/// Metadata for a single member inside a ZIP archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub name: String,
    pub relative_path: PathBuf,
    pub size: u64,
    pub compressed_size: u64,
    pub is_dir: bool,
    pub modified: Option<DateTime<Utc>>,
}

/// Resource limits enforced during archive listing and extraction to defend against zip bombs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveLimits {
    pub max_entries: usize,
    pub max_total_uncompressed_bytes: u64,
    pub max_compression_ratio: u64,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ARCHIVE_ENTRIES,
            max_total_uncompressed_bytes: DEFAULT_MAX_ARCHIVE_UNCOMPRESSED_BYTES,
            max_compression_ratio: DEFAULT_MAX_COMPRESSION_RATIO,
        }
    }
}

/// Asynchronously lists all entries in the ZIP archive at `archive_path` using default [`ArchiveLimits`].
pub async fn list_archive_entries(
    archive_path: PathBuf,
    cancel: CancellationToken,
) -> OnyxResult<Vec<ArchiveEntry>> {
    list_archive_entries_with_limits(archive_path, ArchiveLimits::default(), cancel).await
}

/// Asynchronously lists all entries in the ZIP archive at `archive_path` enforcing `limits`.
pub async fn list_archive_entries_with_limits(
    archive_path: PathBuf,
    limits: ArchiveLimits,
    cancel: CancellationToken,
) -> OnyxResult<Vec<ArchiveEntry>> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    tokio::task::spawn_blocking(move || {
        list_archive_entries_blocking(&archive_path, limits, &cancel)
    })
    .await
    .map_err(|err| OnyxError::OperationFailed {
        message: format!("Archive list task join failed: {err}"),
    })?
}

/// Asynchronously extracts the ZIP archive at `archive_path` into `dest_dir` with strict
/// Zip-Slip traversal protection, default [`ArchiveLimits`], conflict resolution, and progress reporting.
pub async fn extract_archive(
    archive_path: PathBuf,
    dest_dir: PathBuf,
    policy: ConflictPolicy,
    progress: Option<mpsc::Sender<OperationProgress>>,
    cancel: CancellationToken,
) -> OnyxResult<OperationReport> {
    extract_archive_with_limits(
        archive_path,
        dest_dir,
        policy,
        ArchiveLimits::default(),
        progress,
        cancel,
    )
    .await
}

/// Asynchronously extracts the ZIP archive at `archive_path` into `dest_dir` with custom [`ArchiveLimits`].
pub async fn extract_archive_with_limits(
    archive_path: PathBuf,
    dest_dir: PathBuf,
    policy: ConflictPolicy,
    limits: ArchiveLimits,
    progress: Option<mpsc::Sender<OperationProgress>>,
    cancel: CancellationToken,
) -> OnyxResult<OperationReport> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    let op_id = alloc_archive_op_id();
    tokio::task::spawn_blocking(move || {
        extract_archive_blocking(
            op_id,
            &archive_path,
            &dest_dir,
            policy,
            limits,
            progress.as_ref(),
            &cancel,
        )
    })
    .await
    .map_err(|err| OnyxError::OperationFailed {
        message: format!("Archive extract task join failed: {err}"),
    })?
}

/// Asynchronously creates a ZIP archive at `dest_zip` containing `sources` (files and directories
/// recursively), writing atomically via a temporary file and cleaning up on cancellation or error.
pub async fn create_archive(
    sources: Vec<PathBuf>,
    dest_zip: PathBuf,
    progress: Option<mpsc::Sender<OperationProgress>>,
    cancel: CancellationToken,
) -> OnyxResult<OperationReport> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    let op_id = alloc_archive_op_id();
    tokio::task::spawn_blocking(move || {
        create_archive_blocking(op_id, &sources, &dest_zip, progress.as_ref(), &cancel)
    })
    .await
    .map_err(|err| OnyxError::OperationFailed {
        message: format!("Archive create task join failed: {err}"),
    })?
}

fn alloc_archive_op_id() -> u64 {
    NEXT_ARCHIVE_OP_ID.fetch_add(1, Ordering::Relaxed)
}

fn send_progress(tx: Option<&mpsc::Sender<OperationProgress>>, snapshot: OperationProgress) {
    if let Some(sender) = tx {
        if snapshot.finished || snapshot.cancelled || snapshot.error.is_some() {
            let _ = sender.blocking_send(snapshot);
        } else {
            let _ = sender.try_send(snapshot);
        }
    }
}

fn open_zip_file(archive_path: &Path) -> OnyxResult<ZipArchive<File>> {
    let file = match File::open(archive_path) {
        Ok(f) => f,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(OnyxError::NotFound {
                path: archive_path.to_path_buf(),
            });
        }
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(OnyxError::PermissionDenied {
                path: archive_path.to_path_buf(),
            });
        }
        Err(err) => return Err(OnyxError::Io(err)),
    };

    ZipArchive::new(file).map_err(|err| OnyxError::OperationFailed {
        message: format!(
            "Invalid or corrupt ZIP archive '{}': {err}",
            archive_path.display()
        ),
    })
}

/// Validates an archive member's raw name and enclosed path against Zip-Slip traversal attacks.
///
/// Rejects:
/// - Any null byte, drive colon (`:`), or leading slash/backslash
/// - Any `".."` or `"."` segment in raw `/` or `\` splits
/// - Any entry for which `enclosed_name()` returns `None`
/// - Any non-`Component::Normal` component or invalid Windows filename component
fn validate_safe_relative_path(raw_name: &str, enclosed: Option<PathBuf>) -> OnyxResult<PathBuf> {
    if raw_name.is_empty() || raw_name.contains('\0') || raw_name.contains(':') {
        return Err(OnyxError::InvalidPath {
            message: format!("Unsafe archive entry path: '{raw_name}'"),
        });
    }

    if raw_name.starts_with('/') || raw_name.starts_with('\\') {
        return Err(OnyxError::InvalidPath {
            message: format!("Archive entry cannot be an absolute path: '{raw_name}'"),
        });
    }

    for segment in raw_name.split(['/', '\\']) {
        if segment == ".." || segment == "." {
            return Err(OnyxError::InvalidPath {
                message: format!("Zip-Slip path traversal rejected in archive entry: '{raw_name}'"),
            });
        }
    }

    let enclosed_path = enclosed.ok_or_else(|| OnyxError::InvalidPath {
        message: format!("Zip-Slip enclosed_name validation rejected entry: '{raw_name}'"),
    })?;

    let mut clean_rel = PathBuf::new();
    for comp in enclosed_path.components() {
        match comp {
            Component::Normal(os_str) => {
                let part = os_str.to_string_lossy();
                validate_item_name(&part)?;
                clean_rel.push(os_str);
            }
            Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_)
            | Component::CurDir => {
                return Err(OnyxError::InvalidPath {
                    message: format!(
                        "Zip-Slip disallowed path component in archive entry: '{raw_name}'"
                    ),
                });
            }
        }
    }

    if clean_rel.as_os_str().is_empty() {
        return Err(OnyxError::InvalidPath {
            message: format!("Archive entry resolved to an empty relative path: '{raw_name}'"),
        });
    }

    Ok(clean_rel)
}

fn check_entry_compression_ratio(
    entry_name: &str,
    uncompressed_size: u64,
    compressed_size: u64,
    limits: ArchiveLimits,
) -> OnyxResult<()> {
    if limits.max_compression_ratio == 0 || uncompressed_size < RATIO_GUARD_MIN_UNCOMPRESSED_BYTES {
        return Ok(());
    }

    let effective_compressed = compressed_size.max(1);
    let ratio = uncompressed_size / effective_compressed;
    if ratio > limits.max_compression_ratio {
        return Err(OnyxError::OperationFailed {
            message: format!(
                "Archive entry '{entry_name}' exceeds maximum compression ratio ({ratio}:1 > {}:1)",
                limits.max_compression_ratio
            ),
        });
    }
    Ok(())
}

fn zip_datetime_to_utc(dt: Option<zip::DateTime>) -> Option<DateTime<Utc>> {
    let dt = dt?;
    let date = NaiveDate::from_ymd_opt(
        i32::from(dt.year()),
        u32::from(dt.month()),
        u32::from(dt.day()),
    )?;
    let naive_dt = date.and_hms_opt(
        u32::from(dt.hour()),
        u32::from(dt.minute()),
        u32::from(dt.second()),
    )?;
    Some(Utc.from_utc_datetime(&naive_dt))
}

fn list_archive_entries_blocking(
    archive_path: &Path,
    limits: ArchiveLimits,
    cancel: &CancellationToken,
) -> OnyxResult<Vec<ArchiveEntry>> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    let mut archive = open_zip_file(archive_path)?;
    let count = archive.len();
    if count > limits.max_entries {
        return Err(OnyxError::OperationFailed {
            message: format!(
                "Archive contains {count} entries, exceeding limit of {}",
                limits.max_entries
            ),
        });
    }

    let mut entries = Vec::with_capacity(count);
    let mut cumulative_uncompressed = 0u64;

    for idx in 0..count {
        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        let entry = archive
            .by_index(idx)
            .map_err(|err| OnyxError::OperationFailed {
                message: format!("Failed to read archive entry #{idx}: {err}"),
            })?;

        let raw_name = entry.name().to_string();
        let relative_path = validate_safe_relative_path(&raw_name, entry.enclosed_name())?;
        let size = entry.size();
        let compressed_size = entry.compressed_size();

        check_entry_compression_ratio(&raw_name, size, compressed_size, limits)?;
        cumulative_uncompressed = cumulative_uncompressed.saturating_add(size);
        if cumulative_uncompressed > limits.max_total_uncompressed_bytes {
            return Err(OnyxError::OperationFailed {
                message: format!(
                    "Archive total uncompressed size ({cumulative_uncompressed} bytes) exceeds limit of {} bytes",
                    limits.max_total_uncompressed_bytes
                ),
            });
        }

        let name = relative_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| raw_name.trim_end_matches(['/', '\\']).to_string());
        let is_dir = entry.is_dir() || raw_name.ends_with('/') || raw_name.ends_with('\\');
        let modified = zip_datetime_to_utc(entry.last_modified());

        entries.push(ArchiveEntry {
            name,
            relative_path,
            size,
            compressed_size,
            is_dir,
            modified,
        });
    }

    Ok(entries)
}

fn extract_archive_blocking(
    op_id: u64,
    archive_path: &Path,
    dest_dir: &Path,
    policy: ConflictPolicy,
    limits: ArchiveLimits,
    progress: Option<&mpsc::Sender<OperationProgress>>,
    cancel: &CancellationToken,
) -> OnyxResult<OperationReport> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    let mut archive = open_zip_file(archive_path)?;
    let total_items = archive.len();
    if total_items > limits.max_entries {
        return Err(OnyxError::OperationFailed {
            message: format!(
                "Archive contains {total_items} entries, exceeding limit of {}",
                limits.max_entries
            ),
        });
    }

    fs::create_dir_all(dest_dir)?;
    let canon_dest_dir = fs::canonicalize(dest_dir)?;

    // Pre-flight validation pass: verify every entry path is strictly inside `canon_dest_dir`
    // and declared sizes obey `ArchiveLimits` BEFORE writing any files to disk.
    let mut validated_entries = Vec::with_capacity(total_items);
    let mut declared_total_bytes = 0u64;

    for idx in 0..total_items {
        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        let entry = archive
            .by_index(idx)
            .map_err(|err| OnyxError::OperationFailed {
                message: format!("Failed to inspect archive entry #{idx}: {err}"),
            })?;

        let raw_name = entry.name().to_string();
        let rel_path = validate_safe_relative_path(&raw_name, entry.enclosed_name())?;
        let candidate_target = canon_dest_dir.join(&rel_path);

        if !is_same_or_descendant(&canon_dest_dir, &candidate_target)
            || is_same_normalized_path(&canon_dest_dir, &candidate_target)
        {
            return Err(OnyxError::InvalidPath {
                message: format!(
                    "Zip-Slip guard blocked extraction outside destination directory: '{raw_name}'"
                ),
            });
        }

        let size = entry.size();
        let compressed_size = entry.compressed_size();
        check_entry_compression_ratio(&raw_name, size, compressed_size, limits)?;

        declared_total_bytes = declared_total_bytes.saturating_add(size);
        if declared_total_bytes > limits.max_total_uncompressed_bytes {
            return Err(OnyxError::OperationFailed {
                message: format!(
                    "Archive uncompressed total ({declared_total_bytes} bytes) exceeds limit of {} bytes",
                    limits.max_total_uncompressed_bytes
                ),
            });
        }

        let is_dir = entry.is_dir() || raw_name.ends_with('/') || raw_name.ends_with('\\');
        validated_entries.push((idx, raw_name, rel_path, is_dir, compressed_size));
    }

    let mut report = OperationReport::default();
    let mut bytes_extracted = 0u64;
    let mut completed_items = 0usize;
    let mut buf = vec![0u8; ARCHIVE_CHUNK_BYTES];

    for (idx, raw_name, rel_path, is_dir, compressed_size) in validated_entries {
        if cancel.is_cancelled() {
            send_progress(
                progress,
                OperationProgress {
                    op_id,
                    kind: OperationKind::ExtractArchive,
                    current_item: raw_name,
                    completed_items,
                    total_items,
                    bytes_copied: bytes_extracted,
                    total_bytes: declared_total_bytes,
                    finished: true,
                    cancelled: true,
                    error: None,
                },
            );
            return Err(OnyxError::Cancelled);
        }

        let desired_target = dest_dir.join(&rel_path);

        // Ensure no ancestor directory between `dest_dir` and `desired_target` is a symlink/junction.
        ensure_safe_parent_chain(dest_dir, &rel_path)?;

        if is_dir {
            if path_exists_any(&desired_target) {
                let meta = fs::symlink_metadata(&desired_target)?;
                if is_reparse_or_symlink(&meta) {
                    return Err(OnyxError::InvalidPath {
                        message: format!(
                            "Refusing to extract into symlink/reparse directory '{}'",
                            desired_target.display()
                        ),
                    });
                }
                if !meta.is_dir() {
                    match policy {
                        ConflictPolicy::Skip => {
                            report.skipped.push(desired_target);
                            completed_items += 1;
                            continue;
                        }
                        ConflictPolicy::Overwrite => {
                            remove_existing_for_overwrite(&desired_target)?;
                            fs::create_dir_all(&desired_target)?;
                            report.succeeded.push(desired_target);
                        }
                        ConflictPolicy::AutoRename => {
                            let renamed = generate_auto_rename_path(&desired_target)?;
                            fs::create_dir_all(&renamed)?;
                            report.succeeded.push(renamed);
                        }
                        ConflictPolicy::Cancel => return Err(OnyxError::Cancelled),
                        ConflictPolicy::Fail => {
                            return Err(OnyxError::AlreadyExists {
                                path: desired_target,
                            });
                        }
                    }
                } else {
                    report.succeeded.push(desired_target);
                }
            } else {
                fs::create_dir_all(&desired_target)?;
                report.succeeded.push(desired_target);
            }

            completed_items += 1;
            continue;
        }

        // File entry conflict resolution
        let target_path = if path_exists_any(&desired_target) {
            match policy {
                ConflictPolicy::Skip => {
                    report.skipped.push(desired_target);
                    completed_items += 1;
                    continue;
                }
                ConflictPolicy::Overwrite => {
                    remove_existing_for_overwrite(&desired_target)?;
                    desired_target
                }
                ConflictPolicy::AutoRename => generate_auto_rename_path(&desired_target)?,
                ConflictPolicy::Cancel => return Err(OnyxError::Cancelled),
                ConflictPolicy::Fail => {
                    return Err(OnyxError::AlreadyExists {
                        path: desired_target,
                    });
                }
            }
        } else {
            desired_target
        };

        // Final boundary check on resolved target path (including any auto-renamed variant).
        if !is_same_or_descendant(&canon_dest_dir, &target_path) {
            return Err(OnyxError::InvalidPath {
                message: format!(
                    "Target path '{}' escaped destination directory '{}'",
                    target_path.display(),
                    dest_dir.display()
                ),
            });
        }

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut entry = archive
            .by_index(idx)
            .map_err(|err| OnyxError::OperationFailed {
                message: format!("Failed to open archive entry '{raw_name}': {err}"),
            })?;

        let mut out_file = File::create(&target_path)?;
        let mut entry_bytes_written = 0u64;

        loop {
            if cancel.is_cancelled() {
                drop(out_file);
                let _ = fs::remove_file(&target_path);
                send_progress(
                    progress,
                    OperationProgress {
                        op_id,
                        kind: OperationKind::ExtractArchive,
                        current_item: raw_name,
                        completed_items,
                        total_items,
                        bytes_copied: bytes_extracted,
                        total_bytes: declared_total_bytes,
                        finished: true,
                        cancelled: true,
                        error: None,
                    },
                );
                return Err(OnyxError::Cancelled);
            }

            let n = entry
                .read(&mut buf)
                .map_err(|err| OnyxError::OperationFailed {
                    message: format!("Failed reading compressed data for '{raw_name}': {err}"),
                })?;
            if n == 0 {
                break;
            }

            entry_bytes_written = entry_bytes_written.saturating_add(n as u64);
            bytes_extracted = bytes_extracted.saturating_add(n as u64);

            // Runtime streaming guards against spoofed ZIP headers / zip bombs
            if bytes_extracted > limits.max_total_uncompressed_bytes {
                drop(out_file);
                let _ = fs::remove_file(&target_path);
                return Err(OnyxError::OperationFailed {
                    message: format!(
                        "Archive extraction exceeded maximum uncompressed limit of {} bytes",
                        limits.max_total_uncompressed_bytes
                    ),
                });
            }
            if let Err(err) = check_entry_compression_ratio(
                &raw_name,
                entry_bytes_written,
                compressed_size,
                limits,
            ) {
                drop(out_file);
                let _ = fs::remove_file(&target_path);
                return Err(err);
            }

            out_file.write_all(&buf[..n])?;

            send_progress(
                progress,
                OperationProgress {
                    op_id,
                    kind: OperationKind::ExtractArchive,
                    current_item: raw_name.clone(),
                    completed_items,
                    total_items,
                    bytes_copied: bytes_extracted,
                    total_bytes: declared_total_bytes,
                    finished: false,
                    cancelled: false,
                    error: None,
                },
            );
        }

        out_file.flush()?;
        report.succeeded.push(target_path);
        completed_items += 1;

        send_progress(
            progress,
            OperationProgress {
                op_id,
                kind: OperationKind::ExtractArchive,
                current_item: raw_name,
                completed_items,
                total_items,
                bytes_copied: bytes_extracted,
                total_bytes: declared_total_bytes,
                finished: false,
                cancelled: false,
                error: None,
            },
        );
    }

    if !report.succeeded.is_empty() {
        report.undo_action = Some(UndoAction::RecycleCreatedCopies(report.succeeded.clone()));
    }

    send_progress(
        progress,
        OperationProgress {
            op_id,
            kind: OperationKind::ExtractArchive,
            current_item: String::new(),
            completed_items,
            total_items,
            bytes_copied: bytes_extracted,
            total_bytes: declared_total_bytes,
            finished: true,
            cancelled: false,
            error: None,
        },
    );

    Ok(report)
}

/// Ensures every ancestor component under `dest_dir` for `rel_path` is a real directory
/// and never a symlink or Windows reparse point.
fn ensure_safe_parent_chain(dest_dir: &Path, rel_path: &Path) -> OnyxResult<()> {
    let mut cur = dest_dir.to_path_buf();
    if let Some(parent) = rel_path.parent() {
        for comp in parent.components() {
            if let Component::Normal(part) = comp {
                cur.push(part);
                if let Ok(meta) = fs::symlink_metadata(&cur) {
                    if is_reparse_or_symlink(&meta) {
                        return Err(OnyxError::InvalidPath {
                            message: format!(
                                "Zip-Slip symlink ancestor rejected: '{}'",
                                cur.display()
                            ),
                        });
                    }
                    if !meta.is_dir() {
                        return Err(OnyxError::InvalidPath {
                            message: format!(
                                "Cannot extract through non-directory ancestor '{}'",
                                cur.display()
                            ),
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
struct PendingArchiveSource {
    disk_path: PathBuf,
    archive_name: String,
    is_dir: bool,
    size: u64,
}

fn create_archive_blocking(
    op_id: u64,
    sources: &[PathBuf],
    dest_zip: &Path,
    progress: Option<&mpsc::Sender<OperationProgress>>,
    cancel: &CancellationToken,
) -> OnyxResult<OperationReport> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    if sources.is_empty() {
        return Err(OnyxError::InvalidPath {
            message: "Cannot create archive from an empty source list".to_string(),
        });
    }

    let parent_dir = dest_zip.parent().ok_or_else(|| OnyxError::InvalidPath {
        message: format!(
            "Destination archive path '{}' has no parent directory",
            dest_zip.display()
        ),
    })?;
    if !parent_dir.as_os_str().is_empty() {
        fs::create_dir_all(parent_dir)?;
    }

    // Refuse archiving `dest_zip` into itself or archiving a directory that contains `dest_zip`.
    for src in sources {
        let meta = match fs::symlink_metadata(src) {
            Ok(m) => m,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(OnyxError::NotFound { path: src.clone() });
            }
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(OnyxError::PermissionDenied { path: src.clone() });
            }
            Err(err) => return Err(OnyxError::Io(err)),
        };

        if is_same_normalized_path(src, dest_zip)
            || (meta.is_dir()
                && !is_reparse_or_symlink(&meta)
                && is_same_or_descendant(src, dest_zip))
        {
            return Err(OnyxError::RecursiveOperation {
                source_path: src.clone(),
                target_path: dest_zip.to_path_buf(),
            });
        }
    }

    let temp_zip = parent_dir.join(format!(
        ".onyx_archive_tmp_{}_{}.zip",
        std::process::id(),
        op_id
    ));

    // Collect all non-symlink files and directories to archive while preventing directory cycles.
    let mut items = Vec::new();
    let mut visited_dirs = HashSet::new();
    let mut seen_archive_names = HashSet::new();

    for src in sources {
        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }
        let item_name = src
            .file_name()
            .ok_or_else(|| OnyxError::InvalidPath {
                message: format!("Source path '{}' has no file name", src.display()),
            })?
            .to_string_lossy()
            .into_owned();

        collect_archive_items(
            src,
            &item_name,
            dest_zip,
            &temp_zip,
            &mut visited_dirs,
            &mut seen_archive_names,
            &mut items,
            cancel,
        )?;
    }

    let total_items = items.len();
    let total_bytes: u64 = items.iter().map(|i| i.size).sum();

    let result = write_archive_to_temp(
        op_id,
        &items,
        &temp_zip,
        total_items,
        total_bytes,
        progress,
        cancel,
    );

    if let Err(err) = result {
        let _ = fs::remove_file(&temp_zip);
        return Err(err);
    }

    // Atomically replace or move temp archive to `dest_zip`.
    if path_exists_any(dest_zip)
        && let Err(err) = remove_existing_for_overwrite(dest_zip)
    {
        let _ = fs::remove_file(&temp_zip);
        return Err(err);
    }

    if let Err(err) = fs::rename(&temp_zip, dest_zip) {
        let _ = fs::remove_file(&temp_zip);
        return Err(OnyxError::Io(err));
    }

    let report = OperationReport {
        succeeded: vec![dest_zip.to_path_buf()],
        failed: Vec::new(),
        skipped: Vec::new(),
        undo_action: Some(UndoAction::RecycleCreatedCopies(vec![
            dest_zip.to_path_buf(),
        ])),
    };

    send_progress(
        progress,
        OperationProgress {
            op_id,
            kind: OperationKind::CreateArchive,
            current_item: String::new(),
            completed_items: total_items,
            total_items,
            bytes_copied: total_bytes,
            total_bytes,
            finished: true,
            cancelled: false,
            error: None,
        },
    );

    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn collect_archive_items(
    disk_path: &Path,
    archive_rel: &str,
    dest_zip: &Path,
    temp_zip: &Path,
    visited_dirs: &mut HashSet<PathBuf>,
    seen_archive_names: &mut HashSet<String>,
    items: &mut Vec<PendingArchiveSource>,
    cancel: &CancellationToken,
) -> OnyxResult<()> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    if is_same_normalized_path(disk_path, dest_zip) || is_same_normalized_path(disk_path, temp_zip)
    {
        return Ok(());
    }

    let meta = match fs::symlink_metadata(disk_path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(OnyxError::NotFound {
                path: disk_path.to_path_buf(),
            });
        }
        Err(err) => return Err(OnyxError::Io(err)),
    };

    // Never follow symlinks or Windows junctions/reparse points into external or cyclic trees.
    if is_reparse_or_symlink(&meta) {
        tracing::debug!(path = %disk_path.display(), "Skipping symlink/reparse point during archive creation");
        return Ok(());
    }

    let normalized_rel = archive_rel.replace('\\', "/");

    if meta.is_file() {
        if seen_archive_names.insert(normalized_rel.clone()) {
            items.push(PendingArchiveSource {
                disk_path: disk_path.to_path_buf(),
                archive_name: normalized_rel,
                is_dir: false,
                size: meta.len(),
            });
        }
        return Ok(());
    }

    if meta.is_dir() {
        if let Ok(canon) = fs::canonicalize(disk_path)
            && !visited_dirs.insert(canon)
        {
            return Ok(());
        }

        let dir_entry_name = if normalized_rel.ends_with('/') {
            normalized_rel.clone()
        } else {
            format!("{normalized_rel}/")
        };

        if seen_archive_names.insert(dir_entry_name.clone()) {
            items.push(PendingArchiveSource {
                disk_path: disk_path.to_path_buf(),
                archive_name: dir_entry_name,
                is_dir: true,
                size: 0,
            });
        }

        let mut children = Vec::new();
        for entry_res in fs::read_dir(disk_path)? {
            let entry = entry_res?;
            children.push(entry.path());
        }
        children.sort();

        for child_path in children {
            let Some(child_name) = child_path.file_name() else {
                continue;
            };
            let child_rel = format!(
                "{}/{}",
                normalized_rel.trim_end_matches('/'),
                child_name.to_string_lossy()
            );
            collect_archive_items(
                &child_path,
                &child_rel,
                dest_zip,
                temp_zip,
                visited_dirs,
                seen_archive_names,
                items,
                cancel,
            )?;
        }
    }

    Ok(())
}

fn write_archive_to_temp(
    op_id: u64,
    items: &[PendingArchiveSource],
    temp_zip: &Path,
    total_items: usize,
    total_bytes: u64,
    progress: Option<&mpsc::Sender<OperationProgress>>,
    cancel: &CancellationToken,
) -> OnyxResult<()> {
    let temp_file = File::create(temp_zip)?;
    let mut zip = ZipWriter::new(temp_file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .large_file(total_bytes > u64::from(u32::MAX));

    let mut buf = vec![0u8; ARCHIVE_CHUNK_BYTES];
    let mut bytes_written = 0u64;

    for (completed_items, item) in items.iter().enumerate() {
        if cancel.is_cancelled() {
            send_progress(
                progress,
                OperationProgress {
                    op_id,
                    kind: OperationKind::CreateArchive,
                    current_item: item.archive_name.clone(),
                    completed_items,
                    total_items,
                    bytes_copied: bytes_written,
                    total_bytes,
                    finished: true,
                    cancelled: true,
                    error: None,
                },
            );
            return Err(OnyxError::Cancelled);
        }

        if item.is_dir {
            zip.add_directory(&item.archive_name, options)
                .map_err(|err| OnyxError::OperationFailed {
                    message: format!(
                        "Failed adding directory '{}' to archive: {err}",
                        item.archive_name
                    ),
                })?;
        } else {
            zip.start_file(&item.archive_name, options).map_err(|err| {
                OnyxError::OperationFailed {
                    message: format!(
                        "Failed starting archive entry '{}': {err}",
                        item.archive_name
                    ),
                }
            })?;

            let mut reader = File::open(&item.disk_path)?;
            loop {
                if cancel.is_cancelled() {
                    send_progress(
                        progress,
                        OperationProgress {
                            op_id,
                            kind: OperationKind::CreateArchive,
                            current_item: item.archive_name.clone(),
                            completed_items,
                            total_items,
                            bytes_copied: bytes_written,
                            total_bytes,
                            finished: true,
                            cancelled: true,
                            error: None,
                        },
                    );
                    return Err(OnyxError::Cancelled);
                }

                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                zip.write_all(&buf[..n])?;
                bytes_written = bytes_written.saturating_add(n as u64);

                send_progress(
                    progress,
                    OperationProgress {
                        op_id,
                        kind: OperationKind::CreateArchive,
                        current_item: item.archive_name.clone(),
                        completed_items,
                        total_items,
                        bytes_copied: bytes_written,
                        total_bytes,
                        finished: false,
                        cancelled: false,
                        error: None,
                    },
                );
            }
        }
    }

    let mut finished_file = zip.finish().map_err(|err| OnyxError::OperationFailed {
        message: format!("Failed finalizing ZIP archive: {err}"),
    })?;
    finished_file.flush()?;
    Ok(())
}

fn remove_existing_for_overwrite(path: &Path) -> OnyxResult<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(OnyxError::Io(err)),
    };

    if is_reparse_or_symlink(&meta) {
        if is_dir_reparse(&meta) {
            fs::remove_dir(path)?;
        } else {
            fs::remove_file(path)?;
        }
    } else if meta.is_dir() {
        remove_dir_safe_no_follow(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn remove_dir_safe_no_follow(dir: &Path) -> OnyxResult<()> {
    for entry_res in fs::read_dir(dir)? {
        let entry = entry_res?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if is_reparse_or_symlink(&meta) {
            if is_dir_reparse(&meta) {
                fs::remove_dir(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
        } else if meta.is_dir() {
            remove_dir_safe_no_follow(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    fs::remove_dir(dir)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn archive_create_list_and_extract_round_trip_preserves_nested_tree() {
        let temp = TempDir::new().unwrap();
        let src_root = temp.path().join("source_tree");
        let nested = src_root.join("nested").join("deep");
        fs::create_dir_all(&nested).unwrap();

        let top_file = temp.path().join("standalone.txt");
        fs::write(&top_file, b"standalone payload").unwrap();
        fs::write(src_root.join("readme.md"), b"# Project Readme\nLine 2").unwrap();
        fs::write(nested.join("config.toml"), b"key = \"value\"\n").unwrap();

        let zip_path = temp.path().join("bundle.zip");
        let (prog_tx, mut prog_rx) = mpsc::channel(32);

        let create_rep = create_archive(
            vec![top_file.clone(), src_root.clone()],
            zip_path.clone(),
            Some(prog_tx),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(create_rep.succeeded, vec![zip_path.clone()]);
        assert!(zip_path.is_file());

        let mut saw_finished_progress = false;
        while let Ok(p) = prog_rx.try_recv() {
            if p.finished {
                saw_finished_progress = true;
            }
        }
        assert!(saw_finished_progress);

        // List entries
        let entries = list_archive_entries(zip_path.clone(), CancellationToken::new())
            .await
            .unwrap();
        assert!(entries.len() >= 4);
        assert!(
            entries
                .iter()
                .any(|e| e.relative_path == Path::new("standalone.txt") && !e.is_dir)
        );
        assert!(entries.iter().any(|e| {
            e.relative_path
                == Path::new("source_tree")
                    .join("nested")
                    .join("deep")
                    .join("config.toml")
                && e.size == 14
        }));

        // Extract into clean destination
        let extract_dir = temp.path().join("extracted");
        let extract_rep = extract_archive(
            zip_path,
            extract_dir.clone(),
            ConflictPolicy::Fail,
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(extract_rep.failed.is_empty());
        assert_eq!(
            fs::read(extract_dir.join("standalone.txt")).unwrap(),
            b"standalone payload"
        );
        assert_eq!(
            fs::read(extract_dir.join("source_tree").join("readme.md")).unwrap(),
            b"# Project Readme\nLine 2"
        );
        assert_eq!(
            fs::read(
                extract_dir
                    .join("source_tree")
                    .join("nested")
                    .join("deep")
                    .join("config.toml")
            )
            .unwrap(),
            b"key = \"value\"\n"
        );
    }

    #[tokio::test]
    async fn extract_archive_rejects_zip_slip_traversal_attacks() {
        let temp = TempDir::new().unwrap();
        let malicious_zip = temp.path().join("malicious.zip");
        let dest_dir = temp.path().join("safe_dest");
        fs::create_dir_all(&dest_dir).unwrap();

        // Handcraft a malicious zip containing `../evil.txt`
        {
            let file = File::create(&malicious_zip).unwrap();
            let mut zip = ZipWriter::new(file);
            let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            zip.start_file("../evil.txt", opts).unwrap();
            zip.write_all(b"pwned").unwrap();
            zip.finish().unwrap();
        }

        let extract_res = extract_archive(
            malicious_zip.clone(),
            dest_dir.clone(),
            ConflictPolicy::Overwrite,
            None,
            CancellationToken::new(),
        )
        .await;

        assert!(
            matches!(extract_res, Err(OnyxError::InvalidPath { .. })),
            "Expected InvalidPath for Zip-Slip attack, got: {extract_res:?}"
        );
        assert!(
            !temp.path().join("evil.txt").exists(),
            "Zip-Slip payload must never be written outside dest_dir"
        );

        // Also test backslash traversal `sub\..\..\evil2.txt`
        let malicious_backslash_zip = temp.path().join("malicious_backslash.zip");
        {
            let file = File::create(&malicious_backslash_zip).unwrap();
            let mut zip = ZipWriter::new(file);
            let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            zip.start_file(r"sub\..\..\evil2.txt", opts).unwrap();
            zip.write_all(b"pwned2").unwrap();
            zip.finish().unwrap();
        }

        let res2 = extract_archive(
            malicious_backslash_zip,
            dest_dir,
            ConflictPolicy::Overwrite,
            None,
            CancellationToken::new(),
        )
        .await;
        assert!(matches!(res2, Err(OnyxError::InvalidPath { .. })));
        assert!(!temp.path().join("evil2.txt").exists());
    }

    #[tokio::test]
    async fn extract_archive_respects_skip_autorename_overwrite_and_cancel_policies() {
        let temp = TempDir::new().unwrap();
        let src_file = temp.path().join("note.txt");
        fs::write(&src_file, b"from-archive").unwrap();

        let zip_path = temp.path().join("single.zip");
        create_archive(
            vec![src_file],
            zip_path.clone(),
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let dest_dir = temp.path().join("out");
        fs::create_dir_all(&dest_dir).unwrap();
        let existing = dest_dir.join("note.txt");
        fs::write(&existing, b"existing-on-disk").unwrap();

        // 1. Skip -> leaves existing file untouched
        let skip_rep = extract_archive(
            zip_path.clone(),
            dest_dir.clone(),
            ConflictPolicy::Skip,
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(skip_rep.skipped, vec![existing.clone()]);
        assert_eq!(fs::read(&existing).unwrap(), b"existing-on-disk");

        // 2. AutoRename -> extracts as "note (2).txt"
        let rename_rep = extract_archive(
            zip_path.clone(),
            dest_dir.clone(),
            ConflictPolicy::AutoRename,
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let renamed_target = dest_dir.join("note (2).txt");
        assert_eq!(rename_rep.succeeded, vec![renamed_target.clone()]);
        assert_eq!(fs::read(&existing).unwrap(), b"existing-on-disk");
        assert_eq!(fs::read(&renamed_target).unwrap(), b"from-archive");

        // 3. Cancel policy -> aborts with OnyxError::Cancelled on conflict
        let cancel_conflict_res = extract_archive(
            zip_path.clone(),
            dest_dir.clone(),
            ConflictPolicy::Cancel,
            None,
            CancellationToken::new(),
        )
        .await;
        assert!(matches!(cancel_conflict_res, Err(OnyxError::Cancelled)));

        // 4. Overwrite -> replaces existing file content
        let overwrite_rep = extract_archive(
            zip_path,
            dest_dir,
            ConflictPolicy::Overwrite,
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(overwrite_rep.succeeded, vec![existing.clone()]);
        assert_eq!(fs::read(&existing).unwrap(), b"from-archive");
    }

    #[tokio::test]
    async fn archive_operations_respect_cancellation_and_prevent_self_archiving() {
        let temp = TempDir::new().unwrap();
        let f = temp.path().join("data.bin");
        fs::write(&f, vec![42u8; 8192]).unwrap();
        let zip_path = temp.path().join("test.zip");

        // Pre-cancelled token on create_archive
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let err = create_archive(vec![f.clone()], zip_path.clone(), None, cancelled.clone())
            .await
            .unwrap_err();
        assert!(matches!(err, OnyxError::Cancelled));
        assert!(!zip_path.exists());

        // Self-archiving guard: trying to archive `temp.path()` into `temp.path().join("test.zip")`
        let self_err = create_archive(
            vec![temp.path().to_path_buf()],
            zip_path.clone(),
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(self_err, OnyxError::RecursiveOperation { .. }));

        // Create valid archive, then test cancelled list & extract
        create_archive(vec![f], zip_path.clone(), None, CancellationToken::new())
            .await
            .unwrap();

        let list_err = list_archive_entries(zip_path.clone(), cancelled.clone())
            .await
            .unwrap_err();
        assert!(matches!(list_err, OnyxError::Cancelled));

        let extract_err = extract_archive(
            zip_path,
            temp.path().join("extract_cancelled"),
            ConflictPolicy::Overwrite,
            None,
            cancelled,
        )
        .await
        .unwrap_err();
        assert!(matches!(extract_err, OnyxError::Cancelled));
    }
}
