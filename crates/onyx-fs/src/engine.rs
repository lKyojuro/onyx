use onyx_core::{
    FileAttributes, FileEntry, FileKind, GenerationId, OnyxError, OnyxResult, SortSpec,
    infer_type_label, sort::compare_entries, sort_entries, to_extended_length_path,
};
use rayon::slice::ParallelSliceMut;
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

const PARALLEL_SORT_THRESHOLD: usize = 2048;
const DEFAULT_BATCH_SIZE: usize = 256;

#[cfg(windows)]
const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
#[cfg(windows)]
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
#[cfg(windows)]
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
#[cfg(windows)]
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
#[cfg(windows)]
const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x800;
#[cfg(windows)]
const FILE_ATTRIBUTE_ENCRYPTED: u32 = 0x4000;

/// Options controlling directory enumeration filtering, sorting, and batching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnumerationOptions {
    pub show_hidden: bool,
    pub show_system: bool,
    pub sort: SortSpec,
    pub batch_size: usize,
}

impl Default for EnumerationOptions {
    fn default() -> Self {
        Self {
            show_hidden: false,
            show_system: false,
            sort: SortSpec::default(),
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }
}

/// A batch of directory entries delivered during progressive enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirBatch {
    pub generation: GenerationId,
    pub path: PathBuf,
    pub entries: Vec<FileEntry>,
    pub is_final: bool,
    pub total_seen: usize,
}

/// Asynchronous, cancellation-aware filesystem enumeration engine.
#[derive(Debug, Clone, Default)]
pub struct FsEngine;

impl FsEngine {
    /// Creates a new `FsEngine`.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Enumerates all visible entries in `path`, returning a fully sorted [`Vec<FileEntry>`].
    pub async fn enumerate_dir(
        &self,
        path: PathBuf,
        _generation: GenerationId,
        options: EnumerationOptions,
        cancel: CancellationToken,
    ) -> OnyxResult<Vec<FileEntry>> {
        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        tokio::task::spawn_blocking(move || enumerate_dir_blocking(&path, options, &cancel))
            .await
            .map_err(|err| OnyxError::OperationFailed {
                message: format!("enumeration worker join error: {err}"),
            })?
    }

    /// Streams directory entries progressively over `tx` in chunks of `options.batch_size`,
    /// returning the total number of visible entries emitted.
    pub async fn enumerate_dir_batched(
        &self,
        path: PathBuf,
        generation: GenerationId,
        options: EnumerationOptions,
        cancel: CancellationToken,
        tx: mpsc::Sender<DirBatch>,
    ) -> OnyxResult<usize> {
        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        tokio::task::spawn_blocking(move || {
            enumerate_dir_batched_blocking(&path, generation, options, &cancel, &tx)
        })
        .await
        .map_err(|err| OnyxError::OperationFailed {
            message: format!("batched enumeration worker join error: {err}"),
        })?
    }
}

fn enumerate_dir_blocking(
    path: &Path,
    options: EnumerationOptions,
    cancel: &CancellationToken,
) -> OnyxResult<Vec<FileEntry>> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    let extended_dir = to_extended_length_path(path);
    let read_dir = fs::read_dir(&extended_dir).map_err(|err| map_dir_error(err, path))?;

    let mut entries = Vec::new();
    for entry_res in read_dir {
        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        let dir_entry = match entry_res {
            Ok(e) => e,
            Err(err) => {
                warn!(path = %path.display(), error = %err, "Skipping unreadable directory entry");
                continue;
            }
        };

        if let Some(file_entry) = build_file_entry(path, &dir_entry, options) {
            entries.push(file_entry);
        }
    }

    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    sort_entries_smart(&mut entries, options.sort);
    Ok(entries)
}

fn enumerate_dir_batched_blocking(
    path: &Path,
    generation: GenerationId,
    options: EnumerationOptions,
    cancel: &CancellationToken,
    tx: &mpsc::Sender<DirBatch>,
) -> OnyxResult<usize> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    let extended_dir = to_extended_length_path(path);
    let read_dir = fs::read_dir(&extended_dir).map_err(|err| map_dir_error(err, path))?;

    let batch_size = options.batch_size.max(1);
    let mut buffer = Vec::with_capacity(batch_size);
    let mut total_seen = 0usize;

    for entry_res in read_dir {
        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        let dir_entry = match entry_res {
            Ok(e) => e,
            Err(err) => {
                warn!(path = %path.display(), error = %err, "Skipping unreadable directory entry");
                continue;
            }
        };

        let Some(file_entry) = build_file_entry(path, &dir_entry, options) else {
            continue;
        };

        if buffer.len() >= batch_size {
            let mut batch = std::mem::replace(&mut buffer, Vec::with_capacity(batch_size));
            sort_entries_smart(&mut batch, options.sort);
            tx.blocking_send(DirBatch {
                generation,
                path: path.to_path_buf(),
                entries: batch,
                is_final: false,
                total_seen,
            })
            .map_err(|_| OnyxError::Cancelled)?;
        }

        total_seen += 1;
        buffer.push(file_entry);
    }

    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    sort_entries_smart(&mut buffer, options.sort);
    tx.blocking_send(DirBatch {
        generation,
        path: path.to_path_buf(),
        entries: buffer,
        is_final: true,
        total_seen,
    })
    .map_err(|_| OnyxError::Cancelled)?;

    Ok(total_seen)
}

fn build_file_entry(
    parent_dir: &Path,
    dir_entry: &fs::DirEntry,
    options: EnumerationOptions,
) -> Option<FileEntry> {
    let file_name = dir_entry.file_name();
    let entry_path = parent_dir.join(&file_name);
    let extended_entry_path = to_extended_length_path(&entry_path);

    let meta = match fs::symlink_metadata(&extended_entry_path) {
        Ok(m) => m,
        Err(err) => {
            warn!(
                path = %entry_path.display(),
                error = %err,
                "Failed to read symlink_metadata; skipping entry"
            );
            return None;
        }
    };

    let name = file_name.to_string_lossy().into_owned();
    let attributes = extract_attributes(&meta, &name);

    if !options.show_hidden && attributes.hidden {
        return None;
    }
    if !options.show_system && attributes.system {
        return None;
    }

    let file_type = meta.file_type();
    let is_symlink = file_type.is_symlink();
    let symlink_target = if is_symlink {
        fs::read_link(&extended_entry_path).ok()
    } else {
        None
    };

    let kind = if is_symlink {
        FileKind::Symlink
    } else if file_type.is_dir() {
        if attributes.reparse_point {
            FileKind::ReparsePoint
        } else {
            FileKind::Directory
        }
    } else if file_type.is_file() {
        FileKind::File
    } else if attributes.reparse_point {
        FileKind::ReparsePoint
    } else {
        FileKind::Other
    };

    let extension = if matches!(kind, FileKind::Directory | FileKind::ReparsePoint) {
        None
    } else {
        entry_path
            .extension()
            .and_then(|ext| ext.to_str())
            .filter(|ext| !ext.is_empty())
            .map(str::to_owned)
    };

    let size = if kind == FileKind::Directory {
        0
    } else {
        meta.len()
    };

    let modified = system_time_to_unix_secs(meta.modified());
    let created = system_time_to_unix_secs(meta.created());
    let type_label = infer_type_label(kind, extension.as_deref());

    Some(FileEntry {
        path: entry_path,
        name,
        extension,
        kind,
        size,
        modified,
        created,
        attributes,
        is_symlink,
        symlink_target,
        type_label,
    })
}

fn extract_attributes(meta: &Metadata, name: &str) -> FileAttributes {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let _ = name;
        let attrs = meta.file_attributes();
        FileAttributes {
            hidden: (attrs & FILE_ATTRIBUTE_HIDDEN) != 0,
            system: (attrs & FILE_ATTRIBUTE_SYSTEM) != 0,
            readonly: (attrs & FILE_ATTRIBUTE_READONLY) != 0,
            archive: (attrs & FILE_ATTRIBUTE_ARCHIVE) != 0,
            compressed: (attrs & FILE_ATTRIBUTE_COMPRESSED) != 0,
            encrypted: (attrs & FILE_ATTRIBUTE_ENCRYPTED) != 0,
            reparse_point: (attrs & FILE_ATTRIBUTE_REPARSE_POINT) != 0,
        }
    }
    #[cfg(not(windows))]
    {
        FileAttributes {
            hidden: name.starts_with('.'),
            system: false,
            readonly: meta.permissions().readonly(),
            archive: false,
            compressed: false,
            encrypted: false,
            reparse_point: false,
        }
    }
}

fn system_time_to_unix_secs(result: std::io::Result<SystemTime>) -> Option<i64> {
    let time = result.ok()?;
    match time.duration_since(UNIX_EPOCH) {
        Ok(dur) => i64::try_from(dur.as_secs()).ok(),
        Err(err) => i64::try_from(err.duration().as_secs()).ok().map(|s| -s),
    }
}

fn sort_entries_smart(entries: &mut [FileEntry], spec: SortSpec) {
    if entries.len() >= PARALLEL_SORT_THRESHOLD {
        entries.par_sort_by(|a, b| compare_entries(a, b, spec));
    } else {
        sort_entries(entries, spec);
    }
}

fn map_dir_error(err: std::io::Error, path: &Path) -> OnyxError {
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
    async fn enumerate_dir_reads_and_sorts_entries() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("zeta_folder")).unwrap();
        fs::write(tmp.path().join("file10.txt"), b"hello world").unwrap();
        fs::write(tmp.path().join("file2.rs"), b"fn main() {}").unwrap();

        let engine = FsEngine::new();
        let entries = engine
            .enumerate_dir(
                tmp.path().to_path_buf(),
                GenerationId::new(1),
                EnumerationOptions::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "zeta_folder");
        assert_eq!(entries[0].kind, FileKind::Directory);
        assert_eq!(entries[0].type_label, "File folder");

        assert_eq!(entries[1].name, "file2.rs");
        assert_eq!(entries[1].extension.as_deref(), Some("rs"));
        assert_eq!(entries[1].type_label, "Rust Source");

        assert_eq!(entries[2].name, "file10.txt");
        assert_eq!(entries[2].size, 11);
        assert_eq!(entries[2].type_label, "Text Document");
    }

    #[tokio::test]
    async fn enumerate_dir_respects_cancellation() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), b"x").unwrap();

        let cancel = CancellationToken::new();
        cancel.cancel();

        let engine = FsEngine::new();
        let res = engine
            .enumerate_dir(
                tmp.path().to_path_buf(),
                GenerationId::new(2),
                EnumerationOptions::default(),
                cancel,
            )
            .await;

        assert!(matches!(res, Err(OnyxError::Cancelled)));
    }

    #[tokio::test]
    async fn enumerate_dir_not_found_returns_typed_error() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does_not_exist");

        let engine = FsEngine::new();
        let res = engine
            .enumerate_dir(
                missing.clone(),
                GenerationId::new(3),
                EnumerationOptions::default(),
                CancellationToken::new(),
            )
            .await;

        assert!(matches!(res, Err(OnyxError::NotFound { path }) if path == missing));
    }

    #[tokio::test]
    async fn enumerate_dir_batched_streams_multiple_batches() {
        let tmp = TempDir::new().unwrap();
        for i in 0..7 {
            fs::write(tmp.path().join(format!("item_{i}.txt")), b"data").unwrap();
        }

        let (tx, mut rx) = mpsc::channel(8);
        let options = EnumerationOptions {
            batch_size: 3,
            ..EnumerationOptions::default()
        };

        let engine = FsEngine::new();
        let total = engine
            .enumerate_dir_batched(
                tmp.path().to_path_buf(),
                GenerationId::new(7),
                options,
                CancellationToken::new(),
                tx,
            )
            .await
            .unwrap();

        assert_eq!(total, 7);

        let mut batches = Vec::new();
        while let Some(batch) = rx.recv().await {
            batches.push(batch);
        }

        assert_eq!(batches.len(), 3);
        assert!(!batches[0].is_final);
        assert!(!batches[1].is_final);
        assert!(batches[2].is_final);
        assert_eq!(batches[2].total_seen, 7);
        assert_eq!(batches[0].generation, GenerationId::new(7));

        let total_received: usize = batches.iter().map(|b| b.entries.len()).sum();
        assert_eq!(total_received, 7);
    }

    #[tokio::test]
    async fn enumerate_dir_batched_empty_dir_emits_final_batch() {
        let tmp = TempDir::new().unwrap();
        let (tx, mut rx) = mpsc::channel(4);

        let engine = FsEngine::new();
        let total = engine
            .enumerate_dir_batched(
                tmp.path().to_path_buf(),
                GenerationId::new(8),
                EnumerationOptions::default(),
                CancellationToken::new(),
                tx,
            )
            .await
            .unwrap();

        assert_eq!(total, 0);
        let batch = rx.recv().await.unwrap();
        assert!(batch.is_final);
        assert!(batch.entries.is_empty());
        assert_eq!(batch.total_seen, 0);
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn parallel_sort_threshold_sorts_large_directory() {
        let tmp = TempDir::new().unwrap();
        for i in 0..2100 {
            fs::write(tmp.path().join(format!("f{i}.txt")), b"").unwrap();
        }

        let engine = FsEngine::new();
        let entries = engine
            .enumerate_dir(
                tmp.path().to_path_buf(),
                GenerationId::new(9),
                EnumerationOptions::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(entries.len(), 2100);
        assert_eq!(entries[0].name, "f0.txt");
        assert_eq!(entries[1].name, "f1.txt");
        assert_eq!(entries[2].name, "f2.txt");
        assert_eq!(entries[10].name, "f10.txt");
        assert_eq!(entries[2099].name, "f2099.txt");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn hidden_attribute_filtering_on_windows() {
        let tmp = TempDir::new().unwrap();
        let visible = tmp.path().join("visible.txt");
        let hidden = tmp.path().join("secret.txt");
        fs::write(&visible, b"v").unwrap();
        fs::write(&hidden, b"h").unwrap();

        let status = std::process::Command::new("attrib")
            .arg("+H")
            .arg(&hidden)
            .status()
            .unwrap();
        assert!(status.success());

        let engine = FsEngine::new();

        let default_entries = engine
            .enumerate_dir(
                tmp.path().to_path_buf(),
                GenerationId::new(10),
                EnumerationOptions::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(default_entries.len(), 1);
        assert_eq!(default_entries[0].name, "visible.txt");

        let with_hidden = engine
            .enumerate_dir(
                tmp.path().to_path_buf(),
                GenerationId::new(11),
                EnumerationOptions {
                    show_hidden: true,
                    ..EnumerationOptions::default()
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(with_hidden.len(), 2);
        let secret_entry = with_hidden.iter().find(|e| e.name == "secret.txt").unwrap();
        assert!(secret_entry.attributes.hidden);
    }
}
