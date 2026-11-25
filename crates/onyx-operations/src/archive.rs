//! Safe archive listing, extraction (with strict path-traversal, symlink-escape,
//! and decompression-bomb guards), and atomic archive creation off the UI thread.
//!
//! Supports `.zip`, `.tar.gz`, and `.tgz` archives.

use crate::path_guard::{
    generate_auto_rename_path, is_reparse_or_symlink, is_same_normalized_path,
    is_same_or_descendant, validate_item_name,
};
use crate::types::{ConflictPolicy, OperationKind, OperationProgress, OperationReport};
use crate::undo::UndoAction;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use flate2::Compression;
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use onyx_core::{OnyxError, OnyxResult};
use std::cell::RefCell;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;
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

/// Default maximum uncompressed-to-compressed ratio allowed before triggering bomb rejection.
pub const DEFAULT_MAX_COMPRESSION_RATIO: u64 = 200;

/// Minimum uncompressed byte threshold (1 MiB) before applying the compression ratio guard,
/// preventing false positives on small highly repetitive text files.
pub const RATIO_GUARD_MIN_UNCOMPRESSED_BYTES: u64 = 1024 * 1024;

/// Archive container formats supported by Onyx's archive engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArchiveFormat {
    /// Standard ZIP archive (`.zip`).
    Zip,
    /// Gzip-compressed TAR archive (`.tar.gz`, `.tgz`).
    TarGz,
}

impl ArchiveFormat {
    /// Detects the archive format from a file path's extension (case-insensitive).
    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        let file_name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
        if file_name.ends_with(".tar.gz") || file_name.ends_with(".tgz") {
            Some(Self::TarGz)
        } else if file_name.ends_with(".zip") {
            Some(Self::Zip)
        } else {
            None
        }
    }
}

/// Returns `true` if `path` has a supported archive extension (`.zip`, `.tar.gz`, or `.tgz`).
#[must_use]
pub fn is_supported_archive_path(path: &Path) -> bool {
    ArchiveFormat::from_path(path).is_some()
}

/// Metadata for a single member inside an archive.
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

fn open_archive_file(archive_path: &Path) -> OnyxResult<(File, ArchiveFormat)> {
    let mut file = match File::open(archive_path) {
        Ok(f) => f,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(OnyxError::NotFound {
                path: archive_path.to_path_buf(),
            });
        }
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            return Err(OnyxError::PermissionDenied {
                path: archive_path.to_path_buf(),
            });
        }
        Err(err) => return Err(OnyxError::Io(err)),
    };

    if let Some(fmt) = ArchiveFormat::from_path(archive_path) {
        return Ok((file, fmt));
    }

    // Fallback to magic-byte sniffing for archives without a standard extension.
    let mut magic = [0u8; 4];
    let n = file.read(&mut magic)?;
    file.seek(SeekFrom::Start(0))?;

    if n >= 2 && magic[0] == 0x1F && magic[1] == 0x8B {
        Ok((file, ArchiveFormat::TarGz))
    } else if n >= 4
        && (magic == *b"PK\x03\x04" || magic == *b"PK\x05\x06" || magic == *b"PK\x07\x08")
    {
        Ok((file, ArchiveFormat::Zip))
    } else {
        Err(OnyxError::OperationFailed {
            message: format!(
                "Unsupported or unrecognized archive format: '{}'",
                archive_path.display()
            ),
        })
    }
}

fn open_zip_from_file(file: File, archive_path: &Path) -> OnyxResult<ZipArchive<File>> {
    ZipArchive::new(file).map_err(|err| OnyxError::OperationFailed {
        message: format!(
            "Invalid or corrupt ZIP archive '{}': {err}",
            archive_path.display()
        ),
    })
}

/// Bounded, ratio-checked, and cancellable gzip reader used for `.tar.gz` / `.tgz` streams.
struct GuardedGzipReader<'a> {
    decoder: MultiGzDecoder<File>,
    archive_label: String,
    compressed_archive_bytes: u64,
    decompressed_bytes: u64,
    max_stream_bytes: u64,
    limits: ArchiveLimits,
    cancel: &'a CancellationToken,
    abort_error: Rc<RefCell<Option<OnyxError>>>,
}

impl<'a> GuardedGzipReader<'a> {
    fn new(
        file: File,
        archive_path: &Path,
        limits: ArchiveLimits,
        cancel: &'a CancellationToken,
    ) -> OnyxResult<(Self, Rc<RefCell<Option<OnyxError>>>)> {
        let compressed_archive_bytes = file.metadata()?.len();
        let abort_error = Rc::new(RefCell::new(None));
        // Allow tar header/padding overhead above `max_total_uncompressed_bytes` while still
        // capping runaway gzip streams (e.g., gigabytes of zero-block padding with zero entries).
        let tar_overhead = (limits.max_entries.min(DEFAULT_MAX_ARCHIVE_ENTRIES) as u64)
            .saturating_mul(2048)
            .saturating_add(65_536);
        let max_stream_bytes = limits
            .max_total_uncompressed_bytes
            .saturating_add(tar_overhead);

        let reader = Self {
            decoder: MultiGzDecoder::new(file),
            archive_label: archive_path.display().to_string(),
            compressed_archive_bytes,
            decompressed_bytes: 0,
            max_stream_bytes,
            limits,
            cancel,
            abort_error: Rc::clone(&abort_error),
        };
        Ok((reader, abort_error))
    }

    fn record_abort(&self, err: OnyxError) {
        let mut slot = self.abort_error.borrow_mut();
        if slot.is_none() {
            *slot = Some(err);
        }
    }
}

impl Read for GuardedGzipReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            self.record_abort(OnyxError::Cancelled);
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Archive operation cancelled",
            ));
        }

        let n = self.decoder.read(buf)?;
        if n == 0 {
            return Ok(0);
        }

        self.decompressed_bytes = self.decompressed_bytes.saturating_add(n as u64);
        if self.decompressed_bytes > self.max_stream_bytes {
            self.record_abort(OnyxError::OperationFailed {
                message: format!(
                    "Archive '{}' decompressed stream ({} bytes) exceeds maximum limit of {} bytes",
                    self.archive_label,
                    self.decompressed_bytes,
                    self.limits.max_total_uncompressed_bytes
                ),
            });
            return Err(io::Error::other("archive decompressed byte limit exceeded"));
        }

        if let Err(ratio_err) = check_entry_compression_ratio(
            &self.archive_label,
            self.decompressed_bytes,
            self.compressed_archive_bytes,
            self.limits,
        ) {
            self.record_abort(ratio_err);
            return Err(io::Error::other("archive compression ratio exceeded"));
        }

        Ok(n)
    }
}

fn take_abort_or_tar_error(
    abort_error: &Rc<RefCell<Option<OnyxError>>>,
    archive_path: &Path,
    err: io::Error,
) -> OnyxError {
    if let Some(recorded) = abort_error.borrow_mut().take() {
        return recorded;
    }
    OnyxError::OperationFailed {
        message: format!(
            "Invalid or corrupt TAR.GZ archive '{}': {err}",
            archive_path.display()
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TarEntryKind {
    File,
    Dir,
    SkipMetadata,
}

fn classify_tar_entry_type(raw_name: &str, entry_type: tar::EntryType) -> OnyxResult<TarEntryKind> {
    match entry_type {
        tar::EntryType::Regular | tar::EntryType::Continuous => Ok(TarEntryKind::File),
        tar::EntryType::Directory => Ok(TarEntryKind::Dir),
        tar::EntryType::Symlink => Err(OnyxError::InvalidPath {
            message: format!("Refusing symlink entry '{raw_name}' in archive"),
        }),
        tar::EntryType::Link => Err(OnyxError::InvalidPath {
            message: format!("Refusing hardlink entry '{raw_name}' in archive"),
        }),
        tar::EntryType::XGlobalHeader
        | tar::EntryType::XHeader
        | tar::EntryType::GNULongName
        | tar::EntryType::GNULongLink => Ok(TarEntryKind::SkipMetadata),
        other => Err(OnyxError::InvalidPath {
            message: format!("Refusing unsupported special archive entry '{raw_name}' ({other:?})"),
        }),
    }
}

/// Validates a TAR member's raw path bytes against traversal, drive-prefix, and invalid-name attacks.
///
/// Returns `Ok(None)` for harmless archive-root directory markers (`.` or `./`), and
/// `Ok(Some((normalized_raw_name, clean_relative_path)))` for valid entries.
fn validate_tar_entry_path(
    raw_bytes: &[u8],
    is_dir: bool,
) -> OnyxResult<Option<(String, PathBuf)>> {
    if raw_bytes.is_empty() || raw_bytes.contains(&0) {
        return Err(OnyxError::InvalidPath {
            message: "Unsafe archive entry path: empty or contains NUL byte".to_string(),
        });
    }

    let raw_name = std::str::from_utf8(raw_bytes).map_err(|_| OnyxError::InvalidPath {
        message: "Archive entry path is not valid UTF-8".to_string(),
    })?;

    if is_dir && (raw_name == "." || raw_name == "./") {
        return Ok(None);
    }

    // Real-world tarballs created with `tar -czf out.tar.gz .` commonly prefix entries with `./`.
    let candidate = raw_name.strip_prefix("./").unwrap_or(raw_name);
    let clean_rel = validate_safe_relative_path(candidate, Some(PathBuf::from(candidate)))?;
    Ok(Some((candidate.to_string(), clean_rel)))
}

fn tar_mtime_to_utc(mtime: io::Result<u64>) -> Option<DateTime<Utc>> {
    let secs = mtime.ok()?;
    if secs == 0 {
        return None;
    }
    let secs_i64 = i64::try_from(secs).ok()?;
    DateTime::from_timestamp(secs_i64, 0)
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

    let (file, format) = open_archive_file(archive_path)?;
    match format {
        ArchiveFormat::Zip => list_zip_entries_blocking(file, archive_path, limits, cancel),
        ArchiveFormat::TarGz => list_tar_gz_entries_blocking(file, archive_path, limits, cancel),
    }
}

fn list_zip_entries_blocking(
    file: File,
    archive_path: &Path,
    limits: ArchiveLimits,
    cancel: &CancellationToken,
) -> OnyxResult<Vec<ArchiveEntry>> {
    let mut archive = open_zip_from_file(file, archive_path)?;
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

fn list_tar_gz_entries_blocking(
    file: File,
    archive_path: &Path,
    limits: ArchiveLimits,
    cancel: &CancellationToken,
) -> OnyxResult<Vec<ArchiveEntry>> {
    let (reader, abort_error) = GuardedGzipReader::new(file, archive_path, limits, cancel)?;
    let mut archive = tar::Archive::new(reader);

    let tar_entries = archive
        .entries()
        .map_err(|err| take_abort_or_tar_error(&abort_error, archive_path, err))?;

    let mut entries = Vec::new();
    let mut cumulative_uncompressed = 0u64;

    for entry_res in tar_entries {
        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        let entry =
            entry_res.map_err(|err| take_abort_or_tar_error(&abort_error, archive_path, err))?;
        let entry_type = entry.header().entry_type();
        let raw_bytes = entry.path_bytes();
        let raw_preview = String::from_utf8_lossy(&raw_bytes);

        let is_dir = match classify_tar_entry_type(&raw_preview, entry_type)? {
            TarEntryKind::File => false,
            TarEntryKind::Dir => true,
            TarEntryKind::SkipMetadata => continue,
        };

        let Some((raw_name, relative_path)) = validate_tar_entry_path(&raw_bytes, is_dir)? else {
            continue;
        };

        if entries.len() >= limits.max_entries {
            return Err(OnyxError::OperationFailed {
                message: format!(
                    "Archive contains more than {} entries, exceeding limit",
                    limits.max_entries
                ),
            });
        }

        let size = if is_dir {
            0
        } else {
            entry
                .header()
                .size()
                .map_err(|err| take_abort_or_tar_error(&abort_error, archive_path, err))?
        };

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
        let modified = tar_mtime_to_utc(entry.header().mtime());

        entries.push(ArchiveEntry {
            name,
            relative_path,
            size,
            compressed_size: 0,
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

    let (file, format) = open_archive_file(archive_path)?;
    match format {
        ArchiveFormat::Zip => extract_zip_archive_blocking(
            op_id,
            file,
            archive_path,
            dest_dir,
            policy,
            limits,
            progress,
            cancel,
        ),
        ArchiveFormat::TarGz => {
            drop(file);
            extract_tar_gz_archive_blocking(
                op_id,
                archive_path,
                dest_dir,
                policy,
                limits,
                progress,
                cancel,
            )
        }
    }
}

/// Resolves and creates a directory archive entry under `dest_dir`, enforcing conflict policy
/// and symlink safety, without adding pre-existing directories to `report.succeeded`.
fn extract_directory_entry(
    dest_dir: &Path,
    raw_name: &str,
    rel_path: &Path,
    policy: ConflictPolicy,
    report: &mut OperationReport,
    recorded_succeeded: &mut HashSet<PathBuf>,
) -> OnyxResult<()> {
    let desired_target = dest_dir.join(rel_path);
    match fs::symlink_metadata(&desired_target) {
        Ok(meta) => {
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
                    }
                    ConflictPolicy::Overwrite => {
                        return Err(OnyxError::OperationFailed {
                            message: format!(
                                "Cannot overwrite existing file '{}' with directory archive entry '{raw_name}'",
                                desired_target.display()
                            ),
                        });
                    }
                    ConflictPolicy::AutoRename => {
                        if let Some(parent_rel) = rel_path.parent()
                            && !parent_rel.as_os_str().is_empty()
                        {
                            create_missing_dirs_under(
                                dest_dir,
                                parent_rel,
                                &mut report.succeeded,
                                recorded_succeeded,
                            )?;
                        }
                        let renamed = generate_auto_rename_path(&desired_target)?;
                        fs::create_dir(&renamed)?;
                        if recorded_succeeded.insert(renamed.clone()) {
                            report.succeeded.push(renamed);
                        }
                    }
                    ConflictPolicy::Cancel => return Err(OnyxError::Cancelled),
                    ConflictPolicy::Fail => {
                        return Err(OnyxError::AlreadyExists {
                            path: desired_target,
                        });
                    }
                }
            }
            // Pre-existing directory: never push to `report.succeeded` so Undo never recycles it.
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            create_missing_dirs_under(
                dest_dir,
                rel_path,
                &mut report.succeeded,
                recorded_succeeded,
            )?;
        }
        Err(err) => return Err(OnyxError::Io(err)),
    }
    Ok(())
}

/// Resolves the final file target path for a file archive entry, applying `policy` and verifying
/// that the resolved path (including any auto-renamed variant) remains strictly inside `canon_dest_dir`.
///
/// Returns `Ok(None)` if the entry should be skipped under `ConflictPolicy::Skip`.
fn resolve_file_target_path(
    dest_dir: &Path,
    canon_dest_dir: &Path,
    raw_name: &str,
    rel_path: &Path,
    policy: ConflictPolicy,
    report: &mut OperationReport,
) -> OnyxResult<Option<PathBuf>> {
    let desired_target = dest_dir.join(rel_path);
    let target_path = match fs::symlink_metadata(&desired_target) {
        Ok(meta) => match policy {
            ConflictPolicy::Skip => {
                report.skipped.push(desired_target);
                return Ok(None);
            }
            ConflictPolicy::Overwrite => {
                if is_reparse_or_symlink(&meta) {
                    return Err(OnyxError::InvalidPath {
                        message: format!(
                            "Refusing to overwrite symlink or reparse point '{}'",
                            desired_target.display()
                        ),
                    });
                }
                if meta.is_dir() {
                    return Err(OnyxError::OperationFailed {
                        message: format!(
                            "Cannot overwrite existing directory '{}' with file archive entry '{raw_name}'",
                            desired_target.display()
                        ),
                    });
                }
                desired_target
            }
            ConflictPolicy::AutoRename => generate_auto_rename_path(&desired_target)?,
            ConflictPolicy::Cancel => return Err(OnyxError::Cancelled),
            ConflictPolicy::Fail => {
                return Err(OnyxError::AlreadyExists {
                    path: desired_target,
                });
            }
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => desired_target,
        Err(err) => return Err(OnyxError::Io(err)),
    };

    // Final boundary check on resolved target path (including any auto-renamed variant).
    if !is_same_or_descendant(canon_dest_dir, &target_path)
        || is_same_normalized_path(canon_dest_dir, &target_path)
    {
        return Err(OnyxError::InvalidPath {
            message: format!(
                "Target path '{}' escaped destination directory '{}'",
                target_path.display(),
                dest_dir.display()
            ),
        });
    }

    Ok(Some(target_path))
}

#[allow(clippy::too_many_arguments)]
fn extract_zip_archive_blocking(
    op_id: u64,
    file: File,
    archive_path: &Path,
    dest_dir: &Path,
    policy: ConflictPolicy,
    limits: ArchiveLimits,
    progress: Option<&mpsc::Sender<OperationProgress>>,
    cancel: &CancellationToken,
) -> OnyxResult<OperationReport> {
    let mut archive = open_zip_from_file(file, archive_path)?;
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
    let mut recorded_succeeded = HashSet::<PathBuf>::new();
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

        // Ensure no ancestor directory between `dest_dir` and target is a symlink/junction.
        ensure_safe_parent_chain(dest_dir, &rel_path)?;

        if is_dir {
            extract_directory_entry(
                dest_dir,
                &raw_name,
                &rel_path,
                policy,
                &mut report,
                &mut recorded_succeeded,
            )?;
            completed_items += 1;
            continue;
        }

        let Some(target_path) = resolve_file_target_path(
            dest_dir,
            &canon_dest_dir,
            &raw_name,
            &rel_path,
            policy,
            &mut report,
        )?
        else {
            completed_items += 1;
            continue;
        };

        if let Some(parent_rel) = rel_path.parent()
            && !parent_rel.as_os_str().is_empty()
        {
            create_missing_dirs_under(
                dest_dir,
                parent_rel,
                &mut report.succeeded,
                &mut recorded_succeeded,
            )?;
        }

        let target_parent = target_path.parent().ok_or_else(|| OnyxError::InvalidPath {
            message: format!(
                "Target file path '{}' has no parent directory",
                target_path.display()
            ),
        })?;
        let temp_extract_path = target_parent.join(format!(
            ".onyx_extract_tmp_{}_{}_{}",
            std::process::id(),
            op_id,
            idx
        ));

        let extract_res = (|| -> OnyxResult<()> {
            let mut entry = archive
                .by_index(idx)
                .map_err(|err| OnyxError::OperationFailed {
                    message: format!("Failed to open archive entry '{raw_name}': {err}"),
                })?;

            let mut out_file = File::create(&temp_extract_path)?;
            let mut entry_bytes_written = 0u64;

            loop {
                if cancel.is_cancelled() {
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
                    return Err(OnyxError::OperationFailed {
                        message: format!(
                            "Archive extraction exceeded maximum uncompressed limit of {} bytes",
                            limits.max_total_uncompressed_bytes
                        ),
                    });
                }
                check_entry_compression_ratio(
                    &raw_name,
                    entry_bytes_written,
                    compressed_size,
                    limits,
                )?;

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
            out_file.sync_all()?;
            Ok(())
        })();

        if let Err(err) = extract_res {
            let _ = fs::remove_file(&temp_extract_path);
            if matches!(err, OnyxError::Cancelled) {
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
            }
            return Err(err);
        }

        replace_file_atomically(&temp_extract_path, &target_path)?;

        if recorded_succeeded.insert(target_path.clone()) {
            report.succeeded.push(target_path);
        }
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

#[allow(clippy::too_many_arguments)]
fn extract_tar_gz_archive_blocking(
    op_id: u64,
    archive_path: &Path,
    dest_dir: &Path,
    policy: ConflictPolicy,
    limits: ArchiveLimits,
    progress: Option<&mpsc::Sender<OperationProgress>>,
    cancel: &CancellationToken,
) -> OnyxResult<OperationReport> {
    fs::create_dir_all(dest_dir)?;
    let canon_dest_dir = fs::canonicalize(dest_dir)?;

    // Pass 1 (Pre-flight): validate every entry's type, path containment, entry count,
    // declared total uncompressed size, and gzip stream limits BEFORE writing any files to disk.
    let (total_items, declared_total_bytes) = {
        let (preflight_file, _) = open_archive_file(archive_path)?;
        let (reader, abort_error) =
            GuardedGzipReader::new(preflight_file, archive_path, limits, cancel)?;
        let mut archive = tar::Archive::new(reader);
        let tar_entries = archive
            .entries()
            .map_err(|err| take_abort_or_tar_error(&abort_error, archive_path, err))?;

        let mut count = 0usize;
        let mut total_bytes = 0u64;

        for entry_res in tar_entries {
            if cancel.is_cancelled() {
                return Err(OnyxError::Cancelled);
            }

            let entry = entry_res
                .map_err(|err| take_abort_or_tar_error(&abort_error, archive_path, err))?;
            let entry_type = entry.header().entry_type();
            let raw_bytes = entry.path_bytes();
            let raw_preview = String::from_utf8_lossy(&raw_bytes);

            let is_dir = match classify_tar_entry_type(&raw_preview, entry_type)? {
                TarEntryKind::File => false,
                TarEntryKind::Dir => true,
                TarEntryKind::SkipMetadata => continue,
            };

            let Some((raw_name, rel_path)) = validate_tar_entry_path(&raw_bytes, is_dir)? else {
                continue;
            };

            count += 1;
            if count > limits.max_entries {
                return Err(OnyxError::OperationFailed {
                    message: format!(
                        "Archive contains more than {} entries, exceeding limit",
                        limits.max_entries
                    ),
                });
            }

            let candidate_target = canon_dest_dir.join(&rel_path);
            if !is_same_or_descendant(&canon_dest_dir, &candidate_target)
                || is_same_normalized_path(&canon_dest_dir, &candidate_target)
            {
                return Err(OnyxError::InvalidPath {
                    message: format!(
                        "Path-traversal guard blocked extraction outside destination directory: '{raw_name}'"
                    ),
                });
            }

            if !is_dir {
                let size = entry
                    .header()
                    .size()
                    .map_err(|err| take_abort_or_tar_error(&abort_error, archive_path, err))?;
                total_bytes = total_bytes.saturating_add(size);
                if total_bytes > limits.max_total_uncompressed_bytes {
                    return Err(OnyxError::OperationFailed {
                        message: format!(
                            "Archive uncompressed total ({total_bytes} bytes) exceeds limit of {} bytes",
                            limits.max_total_uncompressed_bytes
                        ),
                    });
                }
            }
        }

        (count, total_bytes)
    };

    // Pass 2 (Extraction): stream entries safely into `dest_dir` with atomic temp-file writes,
    // runtime byte/ratio/cancellation guards, conflict resolution, and progress reporting.
    let (extract_file, _) = open_archive_file(archive_path)?;
    let (reader, abort_error) = GuardedGzipReader::new(extract_file, archive_path, limits, cancel)?;
    let mut archive = tar::Archive::new(reader);
    let tar_entries = archive
        .entries()
        .map_err(|err| take_abort_or_tar_error(&abort_error, archive_path, err))?;

    let mut report = OperationReport::default();
    let mut recorded_succeeded = HashSet::<PathBuf>::new();
    let mut bytes_extracted = 0u64;
    let mut completed_items = 0usize;
    let mut entry_idx = 0usize;
    let mut buf = vec![0u8; ARCHIVE_CHUNK_BYTES];

    for entry_res in tar_entries {
        if cancel.is_cancelled() {
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
                    cancelled: true,
                    error: None,
                },
            );
            return Err(OnyxError::Cancelled);
        }

        let mut entry =
            entry_res.map_err(|err| take_abort_or_tar_error(&abort_error, archive_path, err))?;
        let entry_type = entry.header().entry_type();
        let raw_bytes = entry.path_bytes();
        let raw_preview = String::from_utf8_lossy(&raw_bytes);

        let is_dir = match classify_tar_entry_type(&raw_preview, entry_type)? {
            TarEntryKind::File => false,
            TarEntryKind::Dir => true,
            TarEntryKind::SkipMetadata => continue,
        };

        let Some((raw_name, rel_path)) = validate_tar_entry_path(&raw_bytes, is_dir)? else {
            continue;
        };

        entry_idx += 1;
        if entry_idx > limits.max_entries {
            return Err(OnyxError::OperationFailed {
                message: format!(
                    "Archive contains more than {} entries, exceeding limit",
                    limits.max_entries
                ),
            });
        }

        // Ensure no ancestor directory between `dest_dir` and target is a symlink/junction.
        ensure_safe_parent_chain(dest_dir, &rel_path)?;

        if is_dir {
            extract_directory_entry(
                dest_dir,
                &raw_name,
                &rel_path,
                policy,
                &mut report,
                &mut recorded_succeeded,
            )?;
            completed_items += 1;
            continue;
        }

        let Some(target_path) = resolve_file_target_path(
            dest_dir,
            &canon_dest_dir,
            &raw_name,
            &rel_path,
            policy,
            &mut report,
        )?
        else {
            completed_items += 1;
            continue;
        };

        if let Some(parent_rel) = rel_path.parent()
            && !parent_rel.as_os_str().is_empty()
        {
            create_missing_dirs_under(
                dest_dir,
                parent_rel,
                &mut report.succeeded,
                &mut recorded_succeeded,
            )?;
        }

        let target_parent = target_path.parent().ok_or_else(|| OnyxError::InvalidPath {
            message: format!(
                "Target file path '{}' has no parent directory",
                target_path.display()
            ),
        })?;
        let temp_extract_path = target_parent.join(format!(
            ".onyx_extract_tmp_{}_{}_{}",
            std::process::id(),
            op_id,
            entry_idx
        ));

        let extract_res = (|| -> OnyxResult<()> {
            let mut out_file = File::create(&temp_extract_path)?;

            loop {
                if cancel.is_cancelled() {
                    return Err(OnyxError::Cancelled);
                }

                let n = entry
                    .read(&mut buf)
                    .map_err(|err| take_abort_or_tar_error(&abort_error, archive_path, err))?;
                if n == 0 {
                    break;
                }

                bytes_extracted = bytes_extracted.saturating_add(n as u64);
                if bytes_extracted > limits.max_total_uncompressed_bytes {
                    return Err(OnyxError::OperationFailed {
                        message: format!(
                            "Archive extraction exceeded maximum uncompressed limit of {} bytes",
                            limits.max_total_uncompressed_bytes
                        ),
                    });
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
            out_file.sync_all()?;
            Ok(())
        })();

        if let Err(err) = extract_res {
            let _ = fs::remove_file(&temp_extract_path);
            if matches!(err, OnyxError::Cancelled) {
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
            }
            return Err(err);
        }

        replace_file_atomically(&temp_extract_path, &target_path)?;

        if recorded_succeeded.insert(target_path.clone()) {
            report.succeeded.push(target_path);
        }
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

/// Creates missing directory components under `dest_dir` for `rel_dir` one level at a time,
/// refusing symlinks/reparse points and non-directory files, and recording ONLY newly created
/// directories into `succeeded`.
fn create_missing_dirs_under(
    dest_dir: &Path,
    rel_dir: &Path,
    succeeded: &mut Vec<PathBuf>,
    recorded: &mut HashSet<PathBuf>,
) -> OnyxResult<()> {
    let mut cur = dest_dir.to_path_buf();
    for comp in rel_dir.components() {
        if let Component::Normal(part) = comp {
            cur.push(part);
            match fs::symlink_metadata(&cur) {
                Ok(meta) => {
                    if is_reparse_or_symlink(&meta) {
                        return Err(OnyxError::InvalidPath {
                            message: format!(
                                "Zip-Slip symlink directory rejected: '{}'",
                                cur.display()
                            ),
                        });
                    }
                    if !meta.is_dir() {
                        return Err(OnyxError::InvalidPath {
                            message: format!(
                                "Cannot extract through non-directory path '{}'",
                                cur.display()
                            ),
                        });
                    }
                    // Pre-existing directory: never add to `succeeded`.
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&cur)?;
                    if recorded.insert(cur.clone()) {
                        succeeded.push(cur.clone());
                    }
                }
                Err(err) => return Err(OnyxError::Io(err)),
            }
        }
    }
    Ok(())
}

/// Atomically moves a finalized sibling temporary file `temp_path` over `target_path`.
///
/// Never deletes `target_path` beforehand and refuses to overwrite directories or symlinks/reparse points.
fn replace_file_atomically(temp_path: &Path, target_path: &Path) -> OnyxResult<()> {
    match fs::symlink_metadata(target_path) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                let _ = fs::remove_file(temp_path);
                return Err(OnyxError::InvalidPath {
                    message: format!(
                        "Refusing to overwrite symlink or reparse point '{}'",
                        target_path.display()
                    ),
                });
            }
            if meta.is_dir() {
                let _ = fs::remove_file(temp_path);
                return Err(OnyxError::InvalidPath {
                    message: format!(
                        "Refusing to overwrite existing directory '{}' with a file",
                        target_path.display()
                    ),
                });
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            let _ = fs::remove_file(temp_path);
            return Err(OnyxError::Io(err));
        }
    }

    if let Err(primary_err) = fs::rename(temp_path, target_path) {
        if let Ok(meta) = fs::symlink_metadata(target_path)
            && meta.is_file()
            && !is_reparse_or_symlink(&meta)
            && let Some(parent) = target_path.parent()
        {
            let backup_path = parent.join(format!(
                ".onyx_replace_bak_{}_{}",
                std::process::id(),
                alloc_archive_op_id()
            ));
            let _ = fs::remove_file(&backup_path);
            if fs::rename(target_path, &backup_path).is_ok() {
                match fs::rename(temp_path, target_path) {
                    Ok(()) => {
                        let _ = fs::remove_file(&backup_path);
                        return Ok(());
                    }
                    Err(replace_err) => {
                        let _ = fs::rename(&backup_path, target_path);
                        let _ = fs::remove_file(temp_path);
                        return Err(OnyxError::Io(replace_err));
                    }
                }
            }
        }
        let _ = fs::remove_file(temp_path);
        return Err(OnyxError::Io(primary_err));
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

    let dest_name = dest_zip
        .file_name()
        .ok_or_else(|| OnyxError::InvalidPath {
            message: format!(
                "Destination archive path '{}' has no file name",
                dest_zip.display()
            ),
        })?
        .to_string_lossy();
    validate_item_name(&dest_name)?;

    let parent_dir = dest_zip.parent().ok_or_else(|| OnyxError::InvalidPath {
        message: format!(
            "Destination archive path '{}' has no parent directory",
            dest_zip.display()
        ),
    })?;

    // Validate `dest_zip` BEFORE creating any directories or touching disk:
    // 1. Reject if `dest_zip` is an ancestor of (or identical to) any source path.
    for src in sources {
        if is_same_or_descendant(dest_zip, src) {
            return Err(OnyxError::RecursiveOperation {
                source_path: src.clone(),
                target_path: dest_zip.to_path_buf(),
            });
        }
    }

    // 2. Reject if `dest_zip` already exists as a directory or symlink/reparse point.
    match fs::symlink_metadata(dest_zip) {
        Ok(dest_meta) => {
            if is_reparse_or_symlink(&dest_meta) {
                return Err(OnyxError::InvalidPath {
                    message: format!(
                        "Destination archive path '{}' cannot be a symlink or reparse point",
                        dest_zip.display()
                    ),
                });
            }
            if dest_meta.is_dir() {
                return Err(OnyxError::InvalidPath {
                    message: format!(
                        "Destination archive path '{}' is an existing directory",
                        dest_zip.display()
                    ),
                });
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(OnyxError::PermissionDenied {
                path: dest_zip.to_path_buf(),
            });
        }
        Err(err) => return Err(OnyxError::Io(err)),
    }

    // 3. Validate all sources exist and refuse archiving a directory that contains `dest_zip`.
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

    if !parent_dir.as_os_str().is_empty() {
        fs::create_dir_all(parent_dir)?;
    }

    let format = ArchiveFormat::from_path(dest_zip).unwrap_or(ArchiveFormat::Zip);
    let temp_ext = match format {
        ArchiveFormat::Zip => "zip",
        ArchiveFormat::TarGz => "tar.gz",
    };
    let temp_zip = parent_dir.join(format!(
        ".onyx_archive_tmp_{}_{}.{temp_ext}",
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

    let result = match format {
        ArchiveFormat::Zip => write_archive_to_temp(
            op_id,
            &items,
            &temp_zip,
            total_items,
            total_bytes,
            progress,
            cancel,
        ),
        ArchiveFormat::TarGz => write_tar_gz_to_temp(
            op_id,
            &items,
            &temp_zip,
            total_items,
            total_bytes,
            progress,
            cancel,
        ),
    };

    if let Err(err) = result {
        let _ = fs::remove_file(&temp_zip);
        return Err(err);
    }

    // Atomically replace or move finalized temp archive to `dest_zip` without pre-deleting `dest_zip`.
    replace_file_atomically(&temp_zip, dest_zip)?;

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
    finished_file.sync_all()?;
    Ok(())
}

struct ProgressCancelReader<'a, R> {
    inner: R,
    op_id: u64,
    item_name: &'a str,
    completed_items: usize,
    total_items: usize,
    bytes_written: &'a mut u64,
    total_bytes: u64,
    progress: Option<&'a mpsc::Sender<OperationProgress>>,
    cancel: &'a CancellationToken,
    cancelled: &'a mut bool,
}

impl<R: Read> Read for ProgressCancelReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            *self.cancelled = true;
            send_progress(
                self.progress,
                OperationProgress {
                    op_id: self.op_id,
                    kind: OperationKind::CreateArchive,
                    current_item: self.item_name.to_string(),
                    completed_items: self.completed_items,
                    total_items: self.total_items,
                    bytes_copied: *self.bytes_written,
                    total_bytes: self.total_bytes,
                    finished: true,
                    cancelled: true,
                    error: None,
                },
            );
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Archive creation cancelled",
            ));
        }

        let n = self.inner.read(buf)?;
        if n > 0 {
            *self.bytes_written = self.bytes_written.saturating_add(n as u64);
            send_progress(
                self.progress,
                OperationProgress {
                    op_id: self.op_id,
                    kind: OperationKind::CreateArchive,
                    current_item: self.item_name.to_string(),
                    completed_items: self.completed_items,
                    total_items: self.total_items,
                    bytes_copied: *self.bytes_written,
                    total_bytes: self.total_bytes,
                    finished: false,
                    cancelled: false,
                    error: None,
                },
            );
        }
        Ok(n)
    }
}

fn file_mtime_secs(path: &Path) -> u64 {
    fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|sys| sys.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |dur| dur.as_secs())
}

fn write_tar_gz_to_temp(
    op_id: u64,
    items: &[PendingArchiveSource],
    temp_tar_gz: &Path,
    total_items: usize,
    total_bytes: u64,
    progress: Option<&mpsc::Sender<OperationProgress>>,
    cancel: &CancellationToken,
) -> OnyxResult<()> {
    let temp_file = File::create(temp_tar_gz)?;
    let encoder = GzEncoder::new(temp_file, Compression::default());
    let mut builder = tar::Builder::new(encoder);

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

        let mtime = file_mtime_secs(&item.disk_path);

        if item.is_dir {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_mode(0o755);
            header.set_mtime(mtime);
            header.set_cksum();

            builder
                .append_data(&mut header, &item.archive_name, io::empty())
                .map_err(|err| OnyxError::OperationFailed {
                    message: format!(
                        "Failed adding directory '{}' to TAR.GZ archive: {err}",
                        item.archive_name
                    ),
                })?;
        } else {
            let file = File::open(&item.disk_path)?;
            let file_len = file.metadata()?.len();

            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(file_len);
            header.set_mode(0o644);
            header.set_mtime(mtime);
            header.set_cksum();

            let mut was_cancelled = false;
            let mut reader = ProgressCancelReader {
                inner: file.take(file_len),
                op_id,
                item_name: &item.archive_name,
                completed_items,
                total_items,
                bytes_written: &mut bytes_written,
                total_bytes,
                progress,
                cancel,
                cancelled: &mut was_cancelled,
            };

            let append_res = builder.append_data(&mut header, &item.archive_name, &mut reader);
            if was_cancelled {
                return Err(OnyxError::Cancelled);
            }
            append_res.map_err(|err| OnyxError::OperationFailed {
                message: format!(
                    "Failed adding file '{}' to TAR.GZ archive: {err}",
                    item.archive_name
                ),
            })?;
        }
    }

    let encoder = builder
        .into_inner()
        .map_err(|err| OnyxError::OperationFailed {
            message: format!("Failed finalizing TAR stream: {err}"),
        })?;
    let mut finished_file = encoder.finish().map_err(|err| OnyxError::OperationFailed {
        message: format!("Failed finalizing GZIP stream: {err}"),
    })?;
    finished_file.flush()?;
    finished_file.sync_all()?;
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

    #[tokio::test]
    async fn create_archive_rejects_existing_directory_dest_without_deleting_it() {
        let temp = TempDir::new().unwrap();
        let src_file = temp.path().join("source.txt");
        fs::write(&src_file, b"important source").unwrap();

        // Existing directory with user files inside must NEVER be deleted when passed as `dest_zip`.
        let existing_dir = temp.path().join("important_dir.zip");
        fs::create_dir_all(existing_dir.join("sub")).unwrap();
        let canary = existing_dir.join("sub").join("do_not_delete.txt");
        fs::write(&canary, b"precious user data").unwrap();

        let err = create_archive(
            vec![src_file],
            existing_dir.clone(),
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, OnyxError::InvalidPath { .. }),
            "Expected InvalidPath when dest_zip is an existing directory, got: {err:?}"
        );
        assert!(existing_dir.is_dir());
        assert_eq!(fs::read(&canary).unwrap(), b"precious user data");
    }

    #[tokio::test]
    async fn create_archive_rejects_dest_zip_that_is_ancestor_of_source() {
        let temp = TempDir::new().unwrap();
        let ancestor = temp.path().join("workspace_dir");
        let nested_dir = ancestor.join("sub");
        fs::create_dir_all(&nested_dir).unwrap();
        let src_file = nested_dir.join("code.rs");
        fs::write(&src_file, b"fn main() {}").unwrap();

        let err = create_archive(
            vec![src_file.clone()],
            ancestor.clone(),
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, OnyxError::RecursiveOperation { .. }),
            "Expected RecursiveOperation when dest_zip is an ancestor of a source, got: {err:?}"
        );
        assert!(ancestor.is_dir());
        assert_eq!(fs::read(&src_file).unwrap(), b"fn main() {}");
    }

    #[tokio::test]
    async fn create_archive_atomically_replaces_existing_archive_file() {
        let temp = TempDir::new().unwrap();
        let f1 = temp.path().join("v1.txt");
        let f2 = temp.path().join("v2.txt");
        fs::write(&f1, b"version 1").unwrap();
        fs::write(&f2, b"version 2").unwrap();

        let zip_path = temp.path().join("atomic.zip");
        create_archive(vec![f1], zip_path.clone(), None, CancellationToken::new())
            .await
            .unwrap();

        create_archive(vec![f2], zip_path.clone(), None, CancellationToken::new())
            .await
            .unwrap();

        let entries = list_archive_entries(zip_path, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "v2.txt");
    }

    #[tokio::test]
    async fn extract_archive_overwrite_refuses_to_wipe_existing_dir_and_excludes_preexisting_dirs_from_succeeded()
     {
        let temp = TempDir::new().unwrap();

        // 1. Archive containing a file entry `collision` must NEVER wipe an existing directory `collision/`.
        let collision_zip = temp.path().join("collision.zip");
        {
            let file = File::create(&collision_zip).unwrap();
            let mut zip = ZipWriter::new(file);
            let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            zip.start_file("collision", opts).unwrap();
            zip.write_all(b"file payload").unwrap();
            zip.finish().unwrap();
        }

        let dest_dir = temp.path().join("dest");
        let existing_collision_dir = dest_dir.join("collision");
        fs::create_dir_all(&existing_collision_dir).unwrap();
        let canary = existing_collision_dir.join("keep_me.txt");
        fs::write(&canary, b"user folder content").unwrap();

        let err = extract_archive(
            collision_zip,
            dest_dir.clone(),
            ConflictPolicy::Overwrite,
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, OnyxError::OperationFailed { .. }),
            "Expected OperationFailed on file-vs-directory collision, got: {err:?}"
        );
        assert!(existing_collision_dir.is_dir());
        assert_eq!(fs::read(&canary).unwrap(), b"user folder content");

        // 2. Pre-existing directories in `dest_dir` must NEVER be added to `report.succeeded` (or Undo).
        let nested_zip = temp.path().join("nested.zip");
        {
            let file = File::create(&nested_zip).unwrap();
            let mut zip = ZipWriter::new(file);
            let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            zip.add_directory("preexisting_dir/", opts).unwrap();
            zip.start_file("preexisting_dir/new_from_zip.txt", opts)
                .unwrap();
            zip.write_all(b"extracted child").unwrap();
            zip.add_directory("brand_new_dir/", opts).unwrap();
            zip.start_file("brand_new_dir/inside.txt", opts).unwrap();
            zip.write_all(b"inside brand new").unwrap();
            zip.finish().unwrap();
        }

        let dest_dir2 = temp.path().join("dest2");
        let preexisting_dir = dest_dir2.join("preexisting_dir");
        fs::create_dir_all(&preexisting_dir).unwrap();
        let existing_user_file = preexisting_dir.join("user_keep.txt");
        fs::write(&existing_user_file, b"keep").unwrap();

        let rep = extract_archive(
            nested_zip,
            dest_dir2.clone(),
            ConflictPolicy::Overwrite,
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(
            !rep.succeeded.contains(&preexisting_dir),
            "Pre-existing directory must never be recorded in report.succeeded: {:?}",
            rep.succeeded
        );
        assert!(
            rep.succeeded
                .contains(&preexisting_dir.join("new_from_zip.txt"))
        );
        assert!(rep.succeeded.contains(&dest_dir2.join("brand_new_dir")));
        assert!(
            rep.succeeded
                .contains(&dest_dir2.join("brand_new_dir").join("inside.txt"))
        );
        assert_eq!(fs::read(&existing_user_file).unwrap(), b"keep");
    }

    #[tokio::test]
    async fn extract_archive_overwrite_preserves_original_file_and_cleans_temp_on_limit_error() {
        let temp = TempDir::new().unwrap();
        let zip_path = temp.path().join("bomb_like.zip");
        {
            let file = File::create(&zip_path).unwrap();
            let mut zip = ZipWriter::new(file);
            let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            zip.start_file("target.txt", opts).unwrap();
            zip.write_all(&vec![b'A'; 4096]).unwrap();
            zip.finish().unwrap();
        }

        let dest_dir = temp.path().join("dest_atomic");
        fs::create_dir_all(&dest_dir).unwrap();
        let existing_file = dest_dir.join("target.txt");
        fs::write(&existing_file, b"original intact content").unwrap();

        // Use a low uncompressed limit that triggers failure; verify original file is preserved and no temp files remain.
        let strict_limits = ArchiveLimits {
            max_entries: 100,
            max_total_uncompressed_bytes: 1024,
            max_compression_ratio: 200,
        };
        let res = extract_archive_with_limits(
            zip_path,
            dest_dir.clone(),
            ConflictPolicy::Overwrite,
            strict_limits,
            None,
            CancellationToken::new(),
        )
        .await;
        assert!(res.is_err());
        assert_eq!(
            fs::read(&existing_file).unwrap(),
            b"original intact content"
        );

        let leftover_tmp = fs::read_dir(&dest_dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains(".onyx_extract_tmp_")
            });
        assert!(
            !leftover_tmp,
            "Temporary extraction file must be cleaned up on error"
        );

        // Also corrupt the compressed payload bytes inside a valid-header ZIP so failure occurs mid-stream
        // AFTER creating `.onyx_extract_tmp_*`, proving atomic preservation of `existing_file` and temp cleanup.
        let corrupt_zip = temp.path().join("corrupt_crc.zip");
        {
            let file = File::create(&corrupt_zip).unwrap();
            let mut zip = ZipWriter::new(file);
            let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            zip.start_file("target.txt", opts).unwrap();
            zip.write_all(b"payload whose CRC will not match after byte flip")
                .unwrap();
            zip.finish().unwrap();
        }
        let mut raw_zip_bytes = fs::read(&corrupt_zip).unwrap();
        if let Some(pos) = raw_zip_bytes.windows(7).position(|w| w == b"payload") {
            raw_zip_bytes[pos] ^= 0xFF;
        }
        fs::write(&corrupt_zip, &raw_zip_bytes).unwrap();

        let crc_res = extract_archive(
            corrupt_zip,
            dest_dir.clone(),
            ConflictPolicy::Overwrite,
            None,
            CancellationToken::new(),
        )
        .await;
        assert!(crc_res.is_err(), "Corrupt CRC archive must fail extraction");
        assert_eq!(
            fs::read(&existing_file).unwrap(),
            b"original intact content",
            "Existing target file must remain untouched when extraction fails mid-stream"
        );
        let leftover_tmp2 = fs::read_dir(&dest_dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains(".onyx_extract_tmp_")
            });
        assert!(
            !leftover_tmp2,
            "Temporary extraction file must be cleaned up on CRC failure"
        );
    }

    /// Writes a raw 512-byte tar header + padded payload directly into `tar_buf`,
    /// bypassing `tar::Builder` path sanitization so tests can craft hostile paths.
    fn push_raw_tar_entry(
        tar_buf: &mut Vec<u8>,
        raw_path: &str,
        entry_type: tar::EntryType,
        link_target: Option<&str>,
        data: &[u8],
    ) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(entry_type);
        header.set_size(data.len() as u64);
        header.set_mode(if entry_type.is_dir() { 0o755 } else { 0o644 });
        header.set_mtime(1_700_000_000);

        let raw_bytes = header.as_mut_bytes();
        let path_bytes = raw_path.as_bytes();
        assert!(
            path_bytes.len() <= 100,
            "test path exceeds 100-byte header field"
        );
        raw_bytes[..100].fill(0);
        raw_bytes[..path_bytes.len()].copy_from_slice(path_bytes);

        if let Some(target) = link_target {
            let target_bytes = target.as_bytes();
            assert!(
                target_bytes.len() <= 100,
                "test link target exceeds 100-byte header field"
            );
            raw_bytes[157..257].fill(0);
            raw_bytes[157..157 + target_bytes.len()].copy_from_slice(target_bytes);
        }

        header.set_cksum();
        tar_buf.extend_from_slice(header.as_bytes());
        tar_buf.extend_from_slice(data);
        let rem = data.len() % 512;
        if rem != 0 {
            tar_buf.extend(std::iter::repeat_n(0u8, 512 - rem));
        }
    }

    fn write_gzipped_tar(dest: &Path, tar_buf: &[u8]) {
        let mut full_tar = tar_buf.to_vec();
        // Standard 1024-byte EOF zero blocks
        full_tar.extend(std::iter::repeat_n(0u8, 1024));
        let file = File::create(dest).unwrap();
        let mut encoder = GzEncoder::new(file, Compression::default());
        encoder.write_all(&full_tar).unwrap();
        encoder.finish().unwrap();
    }

    #[test]
    fn archive_format_detection_recognizes_zip_tar_gz_and_tgz() {
        assert_eq!(
            ArchiveFormat::from_path(Path::new("bundle.zip")),
            Some(ArchiveFormat::Zip)
        );
        assert_eq!(
            ArchiveFormat::from_path(Path::new("BUNDLE.ZIP")),
            Some(ArchiveFormat::Zip)
        );
        assert_eq!(
            ArchiveFormat::from_path(Path::new("release.tar.gz")),
            Some(ArchiveFormat::TarGz)
        );
        assert_eq!(
            ArchiveFormat::from_path(Path::new("RELEASE.TAR.GZ")),
            Some(ArchiveFormat::TarGz)
        );
        assert_eq!(
            ArchiveFormat::from_path(Path::new("backup.tgz")),
            Some(ArchiveFormat::TarGz)
        );
        assert_eq!(
            ArchiveFormat::from_path(Path::new("BACKUP.TGZ")),
            Some(ArchiveFormat::TarGz)
        );
        assert_eq!(ArchiveFormat::from_path(Path::new("notes.txt")), None);
        assert!(is_supported_archive_path(Path::new("a.tar.gz")));
        assert!(is_supported_archive_path(Path::new("a.tgz")));
        assert!(is_supported_archive_path(Path::new("a.zip")));
        assert!(!is_supported_archive_path(Path::new("a.rar")));
    }

    #[tokio::test]
    async fn tar_gz_and_tgz_create_list_and_extract_round_trip_preserves_nested_tree() {
        let temp = TempDir::new().unwrap();
        let src_root = temp.path().join("project");
        let nested = src_root.join("src").join("nested");
        fs::create_dir_all(&nested).unwrap();

        let top_file = temp.path().join("hello.txt");
        fs::write(&top_file, b"hello tar.gz").unwrap();
        fs::write(src_root.join("README.md"), b"# TarGz Test\n").unwrap();
        fs::write(nested.join("lib.rs"), b"pub fn answer() -> u32 { 42 }\n").unwrap();

        for ext in ["bundle.tar.gz", "bundle.tgz"] {
            let archive_path = temp.path().join(ext);
            let (prog_tx, mut prog_rx) = mpsc::channel(32);

            let create_rep = create_archive(
                vec![top_file.clone(), src_root.clone()],
                archive_path.clone(),
                Some(prog_tx),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(create_rep.succeeded, vec![archive_path.clone()]);
            assert!(archive_path.is_file());

            let mut saw_finished_create = false;
            while let Ok(p) = prog_rx.try_recv() {
                if p.finished {
                    saw_finished_create = true;
                }
            }
            assert!(saw_finished_create);

            let entries = list_archive_entries(archive_path.clone(), CancellationToken::new())
                .await
                .unwrap();
            assert!(
                entries
                    .iter()
                    .any(|e| e.relative_path == Path::new("hello.txt")
                        && !e.is_dir
                        && e.size == 12)
            );
            assert!(entries.iter().any(|e| {
                e.relative_path
                    == Path::new("project")
                        .join("src")
                        .join("nested")
                        .join("lib.rs")
                    && !e.is_dir
                    && e.size == 30
            }));

            let extract_dir = temp.path().join(format!("extracted_{ext}"));
            let (ext_tx, mut ext_rx) = mpsc::channel(32);
            let extract_rep = extract_archive(
                archive_path.clone(),
                extract_dir.clone(),
                ConflictPolicy::Fail,
                Some(ext_tx),
                CancellationToken::new(),
            )
            .await
            .unwrap();

            assert!(extract_rep.failed.is_empty());
            assert!(extract_rep.undo_action.is_some());
            assert_eq!(
                fs::read(extract_dir.join("hello.txt")).unwrap(),
                b"hello tar.gz"
            );
            assert_eq!(
                fs::read(extract_dir.join("project").join("README.md")).unwrap(),
                b"# TarGz Test\n"
            );
            assert_eq!(
                fs::read(
                    extract_dir
                        .join("project")
                        .join("src")
                        .join("nested")
                        .join("lib.rs")
                )
                .unwrap(),
                b"pub fn answer() -> u32 { 42 }\n"
            );

            let mut saw_finished_extract = false;
            while let Ok(p) = ext_rx.try_recv() {
                if p.finished {
                    saw_finished_extract = true;
                }
            }
            assert!(saw_finished_extract);
        }

        // Verify magic-byte fallback works when a `.tar.gz` has a non-standard extension.
        let renamed = temp.path().join("mystery.bin");
        fs::copy(temp.path().join("bundle.tar.gz"), &renamed).unwrap();
        let magic_entries = list_archive_entries(renamed, CancellationToken::new())
            .await
            .unwrap();
        assert!(magic_entries.iter().any(|e| e.name == "hello.txt"));
    }

    #[tokio::test]
    async fn tar_gz_handles_leading_dot_slash_and_root_dir_entries() {
        let temp = TempDir::new().unwrap();
        let archive_path = temp.path().join("dot_slash.tar.gz");

        let mut tar_buf = Vec::new();
        push_raw_tar_entry(&mut tar_buf, "./", tar::EntryType::Directory, None, b"");
        push_raw_tar_entry(&mut tar_buf, "./dir/", tar::EntryType::Directory, None, b"");
        push_raw_tar_entry(
            &mut tar_buf,
            "./dir/file.txt",
            tar::EntryType::Regular,
            None,
            b"from dot slash",
        );
        write_gzipped_tar(&archive_path, &tar_buf);

        let entries = list_archive_entries(archive_path.clone(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .any(|e| { e.relative_path == Path::new("dir").join("file.txt") && !e.is_dir })
        );

        let dest = temp.path().join("out");
        extract_archive(
            archive_path,
            dest.clone(),
            ConflictPolicy::Fail,
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            fs::read(dest.join("dir").join("file.txt")).unwrap(),
            b"from dot slash"
        );
    }

    #[tokio::test]
    async fn tar_gz_rejects_path_traversal_absolute_and_windows_drive_paths() {
        let temp = TempDir::new().unwrap();
        let dest_dir = temp.path().join("safe_dest");
        fs::create_dir_all(&dest_dir).unwrap();

        let malicious_paths = [
            "../evil.txt",
            "sub/../../evil.txt",
            r"sub\..\..\evil.txt",
            "./../evil.txt",
            "/etc/passwd",
            r"\Windows\System32\evil.dll",
            r"C:\evil.txt",
            "C:evil.txt",
            "CON",
            "sub/NUL.txt",
        ];

        for (i, bad_path) in malicious_paths.iter().enumerate() {
            let archive_path = temp.path().join(format!("malicious_{i}.tar.gz"));
            let mut tar_buf = Vec::new();
            // Include a benign entry BEFORE the malicious one to prove pre-flight blocks
            // extraction before writing anything to disk.
            push_raw_tar_entry(
                &mut tar_buf,
                "benign_first.txt",
                tar::EntryType::Regular,
                None,
                b"should not be written",
            );
            push_raw_tar_entry(
                &mut tar_buf,
                bad_path,
                tar::EntryType::Regular,
                None,
                b"pwned",
            );
            write_gzipped_tar(&archive_path, &tar_buf);

            let list_res =
                list_archive_entries(archive_path.clone(), CancellationToken::new()).await;
            assert!(
                matches!(list_res, Err(OnyxError::InvalidPath { .. })),
                "Expected InvalidPath listing '{bad_path}', got: {list_res:?}"
            );

            let extract_res = extract_archive(
                archive_path,
                dest_dir.clone(),
                ConflictPolicy::Overwrite,
                None,
                CancellationToken::new(),
            )
            .await;
            assert!(
                matches!(extract_res, Err(OnyxError::InvalidPath { .. })),
                "Expected InvalidPath extracting '{bad_path}', got: {extract_res:?}"
            );
            assert!(
                !dest_dir.join("benign_first.txt").exists(),
                "Pre-flight must prevent partial extraction when '{bad_path}' is present"
            );
            assert!(!temp.path().join("evil.txt").exists());
        }
    }

    #[tokio::test]
    async fn tar_gz_rejects_symlink_and_hardlink_escape_entries() {
        let temp = TempDir::new().unwrap();
        let dest_dir = temp.path().join("safe_dest");
        fs::create_dir_all(&dest_dir).unwrap();

        // 1. Symlink escape attack (`escape_link -> ../`) followed by `escape_link/pwn.txt`
        let symlink_archive = temp.path().join("symlink_escape.tar.gz");
        {
            let mut tar_buf = Vec::new();
            push_raw_tar_entry(
                &mut tar_buf,
                "escape_link",
                tar::EntryType::Symlink,
                Some("../"),
                b"",
            );
            push_raw_tar_entry(
                &mut tar_buf,
                "escape_link/pwn.txt",
                tar::EntryType::Regular,
                None,
                b"escaped",
            );
            write_gzipped_tar(&symlink_archive, &tar_buf);
        }

        let list_res =
            list_archive_entries(symlink_archive.clone(), CancellationToken::new()).await;
        assert!(
            matches!(list_res, Err(OnyxError::InvalidPath { .. })),
            "Expected InvalidPath for symlink entry in list, got: {list_res:?}"
        );

        let extract_res = extract_archive(
            symlink_archive,
            dest_dir.clone(),
            ConflictPolicy::Overwrite,
            None,
            CancellationToken::new(),
        )
        .await;
        assert!(
            matches!(extract_res, Err(OnyxError::InvalidPath { .. })),
            "Expected InvalidPath for symlink entry in extract, got: {extract_res:?}"
        );
        assert!(!temp.path().join("pwn.txt").exists());

        // 2. Hardlink escape attack
        let hardlink_archive = temp.path().join("hardlink_escape.tgz");
        {
            let mut tar_buf = Vec::new();
            push_raw_tar_entry(
                &mut tar_buf,
                "hard_pwn.txt",
                tar::EntryType::Link,
                Some("../outside.txt"),
                b"",
            );
            write_gzipped_tar(&hardlink_archive, &tar_buf);
        }

        let hard_res = extract_archive(
            hardlink_archive,
            dest_dir,
            ConflictPolicy::Overwrite,
            None,
            CancellationToken::new(),
        )
        .await;
        assert!(
            matches!(hard_res, Err(OnyxError::InvalidPath { .. })),
            "Expected InvalidPath for hardlink entry, got: {hard_res:?}"
        );
    }

    #[tokio::test]
    async fn tar_gz_enforces_entry_count_size_and_compression_ratio_bomb_limits() {
        let temp = TempDir::new().unwrap();

        // 1. Max entries limit
        let many_entries_archive = temp.path().join("many.tar.gz");
        {
            let mut tar_buf = Vec::new();
            for i in 0..5 {
                push_raw_tar_entry(
                    &mut tar_buf,
                    &format!("f_{i}.txt"),
                    tar::EntryType::Regular,
                    None,
                    b"x",
                );
            }
            write_gzipped_tar(&many_entries_archive, &tar_buf);
        }

        let entry_limit = ArchiveLimits {
            max_entries: 3,
            max_total_uncompressed_bytes: 1024 * 1024,
            max_compression_ratio: 200,
        };
        assert!(
            list_archive_entries_with_limits(
                many_entries_archive.clone(),
                entry_limit,
                CancellationToken::new(),
            )
            .await
            .is_err()
        );
        assert!(
            extract_archive_with_limits(
                many_entries_archive,
                temp.path().join("out_entries"),
                ConflictPolicy::Overwrite,
                entry_limit,
                None,
                CancellationToken::new(),
            )
            .await
            .is_err()
        );

        // 2. Max total uncompressed bytes limit (and preserves existing files + leaves no temp files)
        let size_archive = temp.path().join("size_bomb.tar.gz");
        {
            let mut tar_buf = Vec::new();
            push_raw_tar_entry(
                &mut tar_buf,
                "target.txt",
                tar::EntryType::Regular,
                None,
                &vec![b'A'; 4096],
            );
            write_gzipped_tar(&size_archive, &tar_buf);
        }

        let dest_dir = temp.path().join("dest_size");
        fs::create_dir_all(&dest_dir).unwrap();
        let existing = dest_dir.join("target.txt");
        fs::write(&existing, b"keep intact").unwrap();

        let size_limit = ArchiveLimits {
            max_entries: 100,
            max_total_uncompressed_bytes: 1024,
            max_compression_ratio: 200,
        };
        assert!(
            list_archive_entries_with_limits(
                size_archive.clone(),
                size_limit,
                CancellationToken::new(),
            )
            .await
            .is_err()
        );
        assert!(
            extract_archive_with_limits(
                size_archive,
                dest_dir.clone(),
                ConflictPolicy::Overwrite,
                size_limit,
                None,
                CancellationToken::new(),
            )
            .await
            .is_err()
        );
        assert_eq!(fs::read(&existing).unwrap(), b"keep intact");
        let has_tmp = fs::read_dir(&dest_dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains(".onyx_extract_tmp_")
            });
        assert!(!has_tmp);

        // 3. Compression-ratio gzip bomb (2 MiB of zeroes compresses to ~2 KiB -> ratio ~1000:1 > 50:1)
        let ratio_bomb = temp.path().join("ratio_bomb.tgz");
        {
            let mut tar_buf = Vec::new();
            push_raw_tar_entry(
                &mut tar_buf,
                "zeros.bin",
                tar::EntryType::Regular,
                None,
                &vec![0u8; 2 * 1024 * 1024],
            );
            write_gzipped_tar(&ratio_bomb, &tar_buf);
        }
        let ratio_limits = ArchiveLimits {
            max_entries: 100,
            max_total_uncompressed_bytes: 16 * 1024 * 1024,
            max_compression_ratio: 50,
        };
        let ratio_list_res = list_archive_entries_with_limits(
            ratio_bomb.clone(),
            ratio_limits,
            CancellationToken::new(),
        )
        .await;
        assert!(
            matches!(ratio_list_res, Err(OnyxError::OperationFailed { .. })),
            "Expected OperationFailed for gzip ratio bomb in list, got: {ratio_list_res:?}"
        );
        let ratio_extract_res = extract_archive_with_limits(
            ratio_bomb,
            temp.path().join("out_ratio"),
            ConflictPolicy::Overwrite,
            ratio_limits,
            None,
            CancellationToken::new(),
        )
        .await;
        assert!(
            matches!(ratio_extract_res, Err(OnyxError::OperationFailed { .. })),
            "Expected OperationFailed for gzip ratio bomb in extract, got: {ratio_extract_res:?}"
        );
    }

    #[tokio::test]
    async fn tar_gz_respects_conflict_policies_and_preserves_preexisting_dirs() {
        let temp = TempDir::new().unwrap();
        let src_file = temp.path().join("note.txt");
        fs::write(&src_file, b"from-targz").unwrap();

        let archive_path = temp.path().join("single.tgz");
        create_archive(
            vec![src_file],
            archive_path.clone(),
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let dest_dir = temp.path().join("out");
        fs::create_dir_all(&dest_dir).unwrap();
        let existing = dest_dir.join("note.txt");
        fs::write(&existing, b"existing").unwrap();

        // Skip
        let skip_rep = extract_archive(
            archive_path.clone(),
            dest_dir.clone(),
            ConflictPolicy::Skip,
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(skip_rep.skipped, vec![existing.clone()]);
        assert_eq!(fs::read(&existing).unwrap(), b"existing");

        // AutoRename
        let rename_rep = extract_archive(
            archive_path.clone(),
            dest_dir.clone(),
            ConflictPolicy::AutoRename,
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let renamed = dest_dir.join("note (2).txt");
        assert_eq!(rename_rep.succeeded, vec![renamed.clone()]);
        assert_eq!(fs::read(&renamed).unwrap(), b"from-targz");

        // Fail
        let fail_res = extract_archive(
            archive_path.clone(),
            dest_dir.clone(),
            ConflictPolicy::Fail,
            None,
            CancellationToken::new(),
        )
        .await;
        assert!(matches!(fail_res, Err(OnyxError::AlreadyExists { .. })));

        // Cancel
        let cancel_res = extract_archive(
            archive_path.clone(),
            dest_dir.clone(),
            ConflictPolicy::Cancel,
            None,
            CancellationToken::new(),
        )
        .await;
        assert!(matches!(cancel_res, Err(OnyxError::Cancelled)));

        // Overwrite
        let overwrite_rep = extract_archive(
            archive_path,
            dest_dir,
            ConflictPolicy::Overwrite,
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(overwrite_rep.succeeded, vec![existing.clone()]);
        assert_eq!(fs::read(&existing).unwrap(), b"from-targz");
    }

    #[tokio::test]
    async fn tar_gz_respects_cancellation_and_rejects_corrupt_streams() {
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("data.bin");
        fs::write(&src, vec![7u8; 4096]).unwrap();
        let archive_path = temp.path().join("cancel.tar.gz");

        let cancelled = CancellationToken::new();
        cancelled.cancel();

        // Pre-cancelled create
        let create_err = create_archive(
            vec![src.clone()],
            archive_path.clone(),
            None,
            cancelled.clone(),
        )
        .await
        .unwrap_err();
        assert!(matches!(create_err, OnyxError::Cancelled));
        assert!(!archive_path.exists());

        // Create valid archive, then test cancelled list & extract
        create_archive(
            vec![src],
            archive_path.clone(),
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let list_err = list_archive_entries(archive_path.clone(), cancelled.clone())
            .await
            .unwrap_err();
        assert!(matches!(list_err, OnyxError::Cancelled));

        let extract_err = extract_archive(
            archive_path,
            temp.path().join("cancel_out"),
            ConflictPolicy::Overwrite,
            None,
            cancelled,
        )
        .await
        .unwrap_err();
        assert!(matches!(extract_err, OnyxError::Cancelled));

        // Corrupt gzip stream
        let corrupt_path = temp.path().join("corrupt.tar.gz");
        fs::write(&corrupt_path, b"not a valid gzip stream at all").unwrap();
        let corrupt_list = list_archive_entries(corrupt_path.clone(), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(corrupt_list, OnyxError::OperationFailed { .. }));
        let corrupt_extract = extract_archive(
            corrupt_path,
            temp.path().join("corrupt_out"),
            ConflictPolicy::Overwrite,
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(corrupt_extract, OnyxError::OperationFailed { .. }));
    }
}
