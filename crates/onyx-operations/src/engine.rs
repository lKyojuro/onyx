//! Async file operations engine with recursion guards, symlink safety, disk space preflight,
//! cancellation, streaming progress, conflict resolution, partial failure reporting, and undo.

use crate::path_guard::{
    PathConflictOutcome, generate_auto_rename_path, is_dir_reparse, is_reparse_or_symlink,
    is_same_normalized_path, is_same_or_descendant, is_same_volume, path_exists_any,
    resolve_conflict, validate_item_name,
};
use crate::recycler::{Recycler, SpaceChecker, WindowsRecycler, WindowsSpaceChecker};
use crate::types::{
    ConflictInfo, ConflictPolicy, ConflictResolution, ConflictResolver, OperationKind,
    OperationProgress, OperationReport, PermanentDeleteRequest,
};
use crate::undo::{UndoAction, UndoJournal};
use onyx_core::{OnyxError, OnyxResult};
use std::collections::HashSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

const SMALL_FILE_LIMIT: u64 = 4 * 1024 * 1024;

/// Centralized async file operations engine for Onyx.
///
/// All filesystem I/O runs inside `tokio::task::spawn_blocking` so UI and async executor threads
/// are never blocked.
#[derive(Debug, Clone)]
pub struct FileOperationsEngine {
    recycler: Arc<dyn Recycler>,
    space_checker: Arc<dyn SpaceChecker>,
    next_op_id: Arc<AtomicU64>,
    undo_journal: Arc<Mutex<UndoJournal>>,
}

impl Default for FileOperationsEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl FileOperationsEngine {
    /// Creates an engine wired to the real Windows Recycle Bin and Win32 disk-space APIs.
    #[must_use]
    pub fn new() -> Self {
        Self::with_providers(Arc::new(WindowsRecycler), Arc::new(WindowsSpaceChecker))
    }

    /// Creates an engine with a custom [`Recycler`] (recommended for isolated tests).
    #[must_use]
    pub fn with_recycler(recycler: Arc<dyn Recycler>) -> Self {
        Self::with_providers(recycler, Arc::new(WindowsSpaceChecker))
    }

    /// Creates an engine with custom [`Recycler`] and [`SpaceChecker`] providers.
    #[must_use]
    pub fn with_providers(
        recycler: Arc<dyn Recycler>,
        space_checker: Arc<dyn SpaceChecker>,
    ) -> Self {
        Self {
            recycler,
            space_checker,
            next_op_id: Arc::new(AtomicU64::new(1)),
            undo_journal: Arc::new(Mutex::new(UndoJournal::new())),
        }
    }

    fn alloc_op_id(&self) -> u64 {
        self.next_op_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Returns `true` if there is at least one action in the undo journal.
    pub async fn can_undo(&self) -> bool {
        self.undo_journal.lock().await.can_undo()
    }

    /// Returns the number of undo entries currently recorded.
    pub async fn undo_len(&self) -> usize {
        self.undo_journal.lock().await.len()
    }

    async fn record_undo(&self, action: Option<UndoAction>) {
        if let Some(act) = action {
            self.undo_journal.lock().await.push(act);
        }
    }

    /// Copies `sources` into `destination_dir` with recursion protection, disk-space preflight,
    /// symlink safety, cancellable transfers, progress streaming, and conflict handling.
    pub async fn copy_items(
        &self,
        sources: &[PathBuf],
        destination_dir: &Path,
        conflict: ConflictPolicy,
        cancel: CancellationToken,
        progress: Option<mpsc::Sender<OperationProgress>>,
    ) -> OnyxResult<OperationReport> {
        self.copy_items_internal(sources, destination_dir, conflict, None, cancel, progress)
            .await
    }

    /// Copies `sources` into `destination_dir`, invoking `resolver` whenever a destination
    /// collision occurs. Returning `apply_to_all: true` reuses that decision for all remaining
    /// conflicts in this operation.
    pub async fn copy_items_with_resolver<R>(
        &self,
        sources: &[PathBuf],
        destination_dir: &Path,
        default_conflict: ConflictPolicy,
        resolver: R,
        cancel: CancellationToken,
        progress: Option<mpsc::Sender<OperationProgress>>,
    ) -> OnyxResult<OperationReport>
    where
        R: ConflictResolver + 'static,
    {
        self.copy_items_internal(
            sources,
            destination_dir,
            default_conflict,
            Some(Box::new(resolver)),
            cancel,
            progress,
        )
        .await
    }

    async fn copy_items_internal(
        &self,
        sources: &[PathBuf],
        destination_dir: &Path,
        conflict: ConflictPolicy,
        resolver: Option<Box<dyn ConflictResolver>>,
        cancel: CancellationToken,
        progress: Option<mpsc::Sender<OperationProgress>>,
    ) -> OnyxResult<OperationReport> {
        let op_id = self.alloc_op_id();
        let sources = sources.to_vec();
        let destination_dir = destination_dir.to_path_buf();
        let space_checker = Arc::clone(&self.space_checker);

        let report = tokio::task::spawn_blocking(move || {
            copy_items_blocking(
                op_id,
                OperationKind::Copy,
                &sources,
                &destination_dir,
                conflict,
                resolver,
                &cancel,
                progress.as_ref(),
                space_checker.as_ref(),
            )
        })
        .await
        .map_err(|err| OnyxError::OperationFailed {
            message: format!("Copy task join failed: {err}"),
        })??;

        self.record_undo(report.undo_action.clone()).await;
        Ok(report)
    }

    /// Moves `sources` into `destination_dir`, using fast same-volume renames when possible
    /// and falling back to safe copy-then-remove across volumes.
    pub async fn move_items(
        &self,
        sources: &[PathBuf],
        destination_dir: &Path,
        conflict: ConflictPolicy,
        cancel: CancellationToken,
        progress: Option<mpsc::Sender<OperationProgress>>,
    ) -> OnyxResult<OperationReport> {
        self.move_items_internal(sources, destination_dir, conflict, None, cancel, progress)
            .await
    }

    /// Moves `sources` into `destination_dir`, invoking `resolver` whenever a destination
    /// collision occurs. Returning `apply_to_all: true` reuses that decision for all remaining
    /// conflicts in this operation.
    pub async fn move_items_with_resolver<R>(
        &self,
        sources: &[PathBuf],
        destination_dir: &Path,
        default_conflict: ConflictPolicy,
        resolver: R,
        cancel: CancellationToken,
        progress: Option<mpsc::Sender<OperationProgress>>,
    ) -> OnyxResult<OperationReport>
    where
        R: ConflictResolver + 'static,
    {
        self.move_items_internal(
            sources,
            destination_dir,
            default_conflict,
            Some(Box::new(resolver)),
            cancel,
            progress,
        )
        .await
    }

    async fn move_items_internal(
        &self,
        sources: &[PathBuf],
        destination_dir: &Path,
        conflict: ConflictPolicy,
        resolver: Option<Box<dyn ConflictResolver>>,
        cancel: CancellationToken,
        progress: Option<mpsc::Sender<OperationProgress>>,
    ) -> OnyxResult<OperationReport> {
        let op_id = self.alloc_op_id();
        let sources = sources.to_vec();
        let destination_dir = destination_dir.to_path_buf();
        let space_checker = Arc::clone(&self.space_checker);

        let report = tokio::task::spawn_blocking(move || {
            move_items_blocking(
                op_id,
                &sources,
                &destination_dir,
                conflict,
                resolver,
                &cancel,
                progress.as_ref(),
                space_checker.as_ref(),
            )
        })
        .await
        .map_err(|err| OnyxError::OperationFailed {
            message: format!("Move task join failed: {err}"),
        })??;

        self.record_undo(report.undo_action.clone()).await;
        Ok(report)
    }

    /// Renames a single file or directory within its parent folder.
    pub async fn rename_item(
        &self,
        source: &Path,
        new_name: &str,
        conflict: ConflictPolicy,
    ) -> OnyxResult<OperationReport> {
        validate_item_name(new_name)?;
        let source = source.to_path_buf();
        let new_name = new_name.to_string();

        let report =
            tokio::task::spawn_blocking(move || rename_item_blocking(&source, &new_name, conflict))
                .await
                .map_err(|err| OnyxError::OperationFailed {
                    message: format!("Rename task join failed: {err}"),
                })??;

        self.record_undo(report.undo_action.clone()).await;
        Ok(report)
    }

    /// Creates a new directory named `name` inside `parent_dir`.
    pub async fn create_dir(&self, parent_dir: &Path, name: &str) -> OnyxResult<OperationReport> {
        validate_item_name(name)?;
        let parent_dir = parent_dir.to_path_buf();
        let name = name.to_string();

        let report = tokio::task::spawn_blocking(move || create_dir_blocking(&parent_dir, &name))
            .await
            .map_err(|err| OnyxError::OperationFailed {
                message: format!("Create directory task join failed: {err}"),
            })??;

        self.record_undo(report.undo_action.clone()).await;
        Ok(report)
    }

    /// Duplicates `source` in its parent directory using auto-rename naming (`"name (2).ext"`).
    pub async fn duplicate_item(
        &self,
        source: &Path,
        cancel: CancellationToken,
        progress: Option<mpsc::Sender<OperationProgress>>,
    ) -> OnyxResult<OperationReport> {
        let parent_dir = source
            .parent()
            .ok_or_else(|| OnyxError::InvalidPath {
                message: format!(
                    "Cannot determine parent directory of '{}'",
                    source.display()
                ),
            })?
            .to_path_buf();

        let op_id = self.alloc_op_id();
        let sources = vec![source.to_path_buf()];
        let space_checker = Arc::clone(&self.space_checker);

        let report = tokio::task::spawn_blocking(move || {
            copy_items_blocking(
                op_id,
                OperationKind::Duplicate,
                &sources,
                &parent_dir,
                ConflictPolicy::AutoRename,
                None,
                &cancel,
                progress.as_ref(),
                space_checker.as_ref(),
            )
        })
        .await
        .map_err(|err| OnyxError::OperationFailed {
            message: format!("Duplicate task join failed: {err}"),
        })??;

        self.record_undo(report.undo_action.clone()).await;
        Ok(report)
    }

    /// Sends `paths` safely to the Recycle Bin via the configured [`Recycler`].
    pub async fn recycle_items(
        &self,
        paths: &[PathBuf],
        cancel: CancellationToken,
        progress: Option<mpsc::Sender<OperationProgress>>,
    ) -> OnyxResult<OperationReport> {
        let op_id = self.alloc_op_id();
        let paths = paths.to_vec();
        let recycler = Arc::clone(&self.recycler);

        tokio::task::spawn_blocking(move || {
            recycle_items_blocking(op_id, &paths, &cancel, progress.as_ref(), recycler.as_ref())
        })
        .await
        .map_err(|err| OnyxError::OperationFailed {
            message: format!("Recycle task join failed: {err}"),
        })?
    }

    /// Permanently deletes `request.paths` ONLY when `request.confirm_permanent == true`.
    ///
    /// Refuses immediately with [`OnyxError::OperationFailed`] if `confirm_permanent` is `false`.
    pub async fn permanent_delete(
        &self,
        request: PermanentDeleteRequest,
        cancel: CancellationToken,
        progress: Option<mpsc::Sender<OperationProgress>>,
    ) -> OnyxResult<OperationReport> {
        if !request.confirm_permanent {
            return Err(OnyxError::OperationFailed {
                message: "Permanent deletion refused: explicit confirmation (`confirm_permanent = true`) is required".to_string(),
            });
        }

        let op_id = self.alloc_op_id();
        let paths = request.paths;

        tokio::task::spawn_blocking(move || {
            permanent_delete_blocking(op_id, &paths, &cancel, progress.as_ref())
        })
        .await
        .map_err(|err| OnyxError::OperationFailed {
            message: format!("Permanent delete task join failed: {err}"),
        })?
    }

    /// Pops and executes the most recent action from the undo journal.
    ///
    /// Returns `Ok(None)` when there is nothing to undo.
    pub async fn undo_last(&self) -> OnyxResult<Option<OperationReport>> {
        let action = self.undo_journal.lock().await.pop();
        let Some(action) = action else {
            return Ok(None);
        };

        let recycler = Arc::clone(&self.recycler);
        let report =
            tokio::task::spawn_blocking(move || execute_undo_blocking(action, recycler.as_ref()))
                .await
                .map_err(|err| OnyxError::OperationFailed {
                    message: format!("Undo task join failed: {err}"),
                })??;

        Ok(Some(report))
    }
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

/// Safely computes total bytes across `sources`, never descending into symlinks/reparse points
/// and guarding against directory cycles via a visited canonical-path set.
fn measure_total_bytes(sources: &[PathBuf]) -> u64 {
    let mut total = 0u64;
    let mut visited = HashSet::new();
    for src in sources {
        measure_path_bytes(src, &mut visited, &mut total);
    }
    total
}

fn measure_path_bytes(path: &Path, visited: &mut HashSet<PathBuf>, total: &mut u64) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if is_reparse_or_symlink(&meta) {
        return;
    }
    if meta.is_file() {
        *total = total.saturating_add(meta.len());
    } else if meta.is_dir() {
        if let Ok(canon) = fs::canonicalize(path)
            && !visited.insert(canon)
        {
            return;
        }
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                measure_path_bytes(&entry.path(), visited, total);
            }
        }
    }
}

fn check_disk_space(
    space_checker: &dyn SpaceChecker,
    destination_dir: &Path,
    required_bytes: u64,
) -> OnyxResult<()> {
    if required_bytes == 0 {
        return Ok(());
    }
    if let Some(available) = space_checker.free_space(destination_dir)
        && required_bytes > available
    {
        return Err(OnyxError::InsufficientSpace {
            required: required_bytes,
            available,
        });
    }
    Ok(())
}

fn ensure_not_recursive(sources: &[PathBuf], destination_dir: &Path) -> OnyxResult<()> {
    for src in sources {
        if let Ok(meta) = fs::symlink_metadata(src)
            && meta.is_dir()
            && !is_reparse_or_symlink(&meta)
            && is_same_or_descendant(src, destination_dir)
        {
            return Err(OnyxError::RecursiveOperation {
                source_path: src.clone(),
                target_path: destination_dir.to_path_buf(),
            });
        }
    }
    Ok(())
}

fn resolve_item_conflict(
    src: &Path,
    desired_target: &Path,
    default_policy: ConflictPolicy,
    resolver: &mut Option<Box<dyn ConflictResolver>>,
    sticky_resolution: &mut Option<ConflictResolution>,
) -> OnyxResult<PathConflictOutcome> {
    if !path_exists_any(desired_target) {
        return Ok(PathConflictOutcome::Proceed(desired_target.to_path_buf()));
    }

    let effective_policy = if let Some(resolution) = *sticky_resolution {
        ConflictPolicy::from(resolution)
    } else if let Some(res_fn) = resolver.as_deref_mut() {
        let info = ConflictInfo {
            source: src.to_path_buf(),
            destination: desired_target.to_path_buf(),
            is_dir: fs::symlink_metadata(desired_target).is_ok_and(|m| m.is_dir()),
        };
        let decision = res_fn.resolve(&info);
        if decision.apply_to_all {
            *sticky_resolution = Some(decision.resolution);
        }
        ConflictPolicy::from(decision.resolution)
    } else {
        default_policy
    };

    resolve_conflict(src, desired_target, effective_policy, false)
}

#[allow(clippy::too_many_arguments)]
fn copy_items_blocking(
    op_id: u64,
    kind: OperationKind,
    sources: &[PathBuf],
    destination_dir: &Path,
    conflict: ConflictPolicy,
    mut resolver: Option<Box<dyn ConflictResolver>>,
    cancel: &CancellationToken,
    progress: Option<&mpsc::Sender<OperationProgress>>,
    space_checker: &dyn SpaceChecker,
) -> OnyxResult<OperationReport> {
    if !destination_dir.is_dir() {
        return Err(OnyxError::NotFound {
            path: destination_dir.to_path_buf(),
        });
    }

    ensure_not_recursive(sources, destination_dir)?;
    let total_bytes = measure_total_bytes(sources);
    check_disk_space(space_checker, destination_dir, total_bytes)?;

    let mut transfer = TransferProgress::new(op_id, kind, sources.len(), total_bytes, progress);
    let workers = onyx_windows::recommended_transfer_workers(
        sources.first().map_or(destination_dir, PathBuf::as_path),
        destination_dir,
    );
    let mut report = OperationReport::default();
    let mut sticky_resolution = None;
    let mut batch = CopyBatch::default();

    let result = (|| {
        for src in sources {
            check_cancelled(cancel)?;

            let planned = (|| {
                let item_name = src.file_name().ok_or_else(|| OnyxError::InvalidPath {
                    message: "Source path has no file name".into(),
                })?;
                let desired = destination_dir.join(item_name);
                let name_key = item_name
                    .to_string_lossy()
                    .trim_end_matches(['.', ' '])
                    .to_uppercase();
                // Conflicts must see the effects of preceding entries, including auto-renames.
                if batch.names.contains(&name_key)
                    || path_exists_any(&desired)
                    || src.starts_with(destination_dir)
                {
                    batch.execute(workers, cancel, &mut transfer, &mut report)?;
                }
                let meta = fs::symlink_metadata(src)?;
                let target = match resolve_item_conflict(
                    src,
                    &desired,
                    conflict,
                    &mut resolver,
                    &mut sticky_resolution,
                )? {
                    PathConflictOutcome::Skip => return Ok(None),
                    PathConflictOutcome::Proceed(path) => path,
                    PathConflictOutcome::Overwrite(path) => {
                        remove_existing_for_overwrite(&path)?;
                        path
                    }
                };

                let mut plan = CopyPlan {
                    files: &mut batch.files,
                    top_idx: batch.items.len(),
                    cancel,
                    require_complete: false,
                    visited: HashSet::new(),
                };
                let outcome = plan.entry(src, &target, &meta);
                if let Some(name) = target.file_name() {
                    batch.names.insert(
                        name.to_string_lossy()
                            .trim_end_matches(['.', ' '])
                            .to_uppercase(),
                    );
                }
                outcome.map(|outcome| (outcome == CopyOutcome::Copied).then_some(target))
            })();
            if matches!(planned, Err(OnyxError::Cancelled)) {
                return Err(OnyxError::Cancelled);
            }
            batch.items.push((src, planned));
        }

        batch.execute(workers, cancel, &mut transfer, &mut report)
    })();

    transfer.emit("", true, result.is_err());
    result?;
    if !report.succeeded.is_empty() {
        report.undo_action = Some(UndoAction::RecycleCreatedCopies(report.succeeded.clone()));
    }
    Ok(report)
}

fn check_cancelled(cancel: &CancellationToken) -> OnyxResult<()> {
    if cancel.is_cancelled() {
        Err(OnyxError::Cancelled)
    } else {
        Ok(())
    }
}

#[derive(Default)]
struct CopyBatch<'a> {
    names: HashSet<String>,
    files: Vec<PlannedFile>,
    items: Vec<(&'a Path, OnyxResult<Option<PathBuf>>)>,
}

impl CopyBatch<'_> {
    fn execute(
        &mut self,
        workers: usize,
        cancel: &CancellationToken,
        progress: &mut TransferProgress<'_>,
        report: &mut OperationReport,
    ) -> OnyxResult<()> {
        if self.items.is_empty() {
            return Ok(());
        }
        let failures = execute_copy_plan(&self.files, self.items.len(), workers, cancel, progress)?;
        for ((source, planned), failure) in self.items.drain(..).zip(failures) {
            match planned.and_then(|target| failure.map_or(Ok(target), Err)) {
                Ok(Some(target)) => report.succeeded.push(target),
                Ok(None) => report.skipped.push(source.to_path_buf()),
                Err(err) => report.failed.push((source.to_path_buf(), err.to_string())),
            }
            progress.complete(source);
        }
        self.files.clear();
        self.names.clear();
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyOutcome {
    Copied,
    Skipped,
}

struct PlannedFile {
    src: PathBuf,
    dst: PathBuf,
    size: u64,
    top_idx: usize,
}

struct CopyPlan<'a> {
    files: &'a mut Vec<PlannedFile>,
    top_idx: usize,
    cancel: &'a CancellationToken,
    require_complete: bool,
    visited: HashSet<PathBuf>,
}

impl CopyPlan<'_> {
    fn entry(&mut self, src: &Path, dst: &Path, meta: &fs::Metadata) -> OnyxResult<CopyOutcome> {
        check_cancelled(self.cancel)?;
        if is_reparse_or_symlink(meta) {
            return copy_symlink_safe(src, dst, meta);
        }

        if meta.is_file() {
            self.files.push(PlannedFile {
                src: src.to_path_buf(),
                dst: dst.to_path_buf(),
                size: meta.len(),
                top_idx: self.top_idx,
            });
            return Ok(CopyOutcome::Copied);
        }

        if !meta.is_dir() {
            return Ok(CopyOutcome::Skipped);
        }
        if let Ok(canon) = fs::canonicalize(src)
            && !self.visited.insert(canon)
        {
            return Ok(CopyOutcome::Skipped);
        }

        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            check_cancelled(self.cancel)?;
            let entry = entry?;
            let child = entry.path();
            let child_meta = fs::symlink_metadata(&child)?;
            let outcome = self.entry(&child, &dst.join(entry.file_name()), &child_meta)?;
            if self.require_complete && outcome == CopyOutcome::Skipped {
                return Err(OnyxError::OperationFailed {
                    message: format!(
                        "Could not copy '{}' before removing its source",
                        child.display()
                    ),
                });
            }
        }
        Ok(CopyOutcome::Copied)
    }
}

struct TransferProgress<'a> {
    op_id: u64,
    kind: OperationKind,
    total_items: usize,
    total_bytes: u64,
    bytes: AtomicU64,
    completed: usize,
    last_update: StdMutex<Option<Instant>>,
    sender: Option<&'a mpsc::Sender<OperationProgress>>,
}

impl<'a> TransferProgress<'a> {
    fn new(
        op_id: u64,
        kind: OperationKind,
        total_items: usize,
        total_bytes: u64,
        sender: Option<&'a mpsc::Sender<OperationProgress>>,
    ) -> Self {
        Self {
            op_id,
            kind,
            total_items,
            total_bytes,
            bytes: AtomicU64::new(0),
            completed: 0,
            last_update: StdMutex::new(None),
            sender,
        }
    }

    fn complete(&mut self, source: &Path) {
        self.completed += 1;
        let label = source.file_name().unwrap_or(source.as_os_str());
        self.emit(&label.to_string_lossy(), false, false);
    }

    fn add_bytes(&self, source: &Path, bytes: u64) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        if self.sender.is_none() {
            return;
        }
        let mut last = self
            .last_update
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if last.is_none_or(|time| time.elapsed() >= Duration::from_millis(40)) {
            *last = Some(Instant::now());
            let label = source.file_name().unwrap_or(source.as_os_str());
            self.emit(&label.to_string_lossy(), false, false);
        }
    }

    fn emit(&self, current_item: &str, finished: bool, cancelled: bool) {
        send_progress(
            self.sender,
            OperationProgress {
                op_id: self.op_id,
                kind: self.kind,
                current_item: current_item.to_owned(),
                completed_items: self.completed,
                total_items: self.total_items,
                bytes_copied: self.bytes.load(Ordering::Relaxed),
                total_bytes: self.total_bytes,
                finished,
                cancelled,
                error: None,
            },
        );
    }
}

fn execute_copy_plan(
    files: &[PlannedFile],
    top_count: usize,
    workers: usize,
    cancel: &CancellationToken,
    progress: &TransferProgress<'_>,
) -> OnyxResult<Vec<Option<OnyxError>>> {
    let cancel = cancel.child_token();
    let failures: Vec<_> = (0..top_count).map(|_| StdMutex::new(None)).collect();
    let small: Vec<_> = files
        .iter()
        .filter(|file| file.size < SMALL_FILE_LIMIT)
        .collect();
    let parallel = workers > 1 && small.len() >= 4;

    let copy = |file: &PlannedFile, tiny_fast_path| {
        let failure = &failures[file.top_idx];
        if failure
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .is_some()
        {
            return;
        }
        let result = transfer_file(file, tiny_fast_path, &cancel, progress);
        match result {
            Ok(()) => {}
            Err(OnyxError::Cancelled) => cancel.cancel(),
            Err(err) => {
                failure
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .get_or_insert(err);
            }
        }
    };

    if parallel {
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..workers.min(small.len()) {
                scope.spawn(|| {
                    while !cancel.is_cancelled() {
                        let Some(file) = small.get(next.fetch_add(1, Ordering::Relaxed)) else {
                            break;
                        };
                        copy(file, true);
                    }
                });
            }
        });
    }
    for file in files {
        check_cancelled(&cancel)?;
        if !parallel || file.size >= SMALL_FILE_LIMIT {
            copy(file, false);
        }
    }
    check_cancelled(&cancel)?;
    Ok(failures
        .into_iter()
        .map(|failure| failure.into_inner().unwrap_or_else(|err| err.into_inner()))
        .collect())
}

fn transfer_file(
    file: &PlannedFile,
    tiny_fast_path: bool,
    cancel: &CancellationToken,
    progress: &TransferProgress<'_>,
) -> OnyxResult<()> {
    check_cancelled(cancel)?;
    let meta = fs::symlink_metadata(&file.src)?;
    if !meta.is_file() || is_reparse_or_symlink(&meta) {
        return Err(OnyxError::InvalidPath {
            message: format!("Copy source changed type: '{}'", file.src.display()),
        });
    }
    let size = meta.len();
    if onyx_windows::try_clone_file_refs(&file.src, &file.dst)? {
        progress.add_bytes(&file.src, size);
        return Ok(());
    }

    if tiny_fast_path && size < 128 * 1024 {
        let done = copy_tiny_file(&file.src, &file.dst, &meta, cancel)?;
        progress.add_bytes(&file.src, done);
        return Ok(());
    }

    let mut reported = 0;
    onyx_windows::copy_file_fast(
        &file.src,
        &file.dst,
        onyx_windows::CopyFileOptions {
            no_buffering: size >= 8 * 1024 * 1024,
            // The planner already resolved overwrite. Protect files created after planning.
            overwrite: false,
        },
        || cancel.is_cancelled(),
        |done, _| {
            if done > reported {
                progress.add_bytes(&file.src, done - reported);
                reported = done;
            }
        },
    )?;
    if size > reported {
        progress.add_bytes(&file.src, size - reported);
    }
    Ok(())
}

fn copy_tiny_file(
    src: &Path,
    dst: &Path,
    meta: &fs::Metadata,
    cancel: &CancellationToken,
) -> OnyxResult<u64> {
    let mut reader = File::open(src)?;
    let mut writer = File::create_new(dst)?;
    let result = (|| {
        let preallocated =
            meta.len() >= 64 * 1024 && onyx_windows::preallocate_file(&writer, meta.len()).is_ok();
        let done = std::io::copy(&mut reader, &mut writer)?;
        if preallocated && done < meta.len() {
            writer.set_len(done)?;
        }
        if let Ok(modified) = meta.modified() {
            let _ = writer.set_modified(modified);
        }
        check_cancelled(cancel)?;
        Ok(done)
    })();
    drop(writer);
    if result.is_err() {
        let _ = fs::remove_file(dst);
    }
    result
}

fn copy_symlink_safe(src: &Path, dst: &Path, meta: &fs::Metadata) -> OnyxResult<CopyOutcome> {
    let Ok(target) = fs::read_link(src) else {
        // Reparse point that isn't a readable symlink (e.g., unsupported junction/cloud stub): skip safely.
        tracing::debug!(path = %src.display(), "Skipping unreadable reparse point during copy");
        return Ok(CopyOutcome::Skipped);
    };

    #[cfg(windows)]
    let link_res = {
        use std::os::windows::fs::{symlink_dir, symlink_file};
        if is_dir_reparse(meta) {
            symlink_dir(&target, dst)
        } else {
            symlink_file(&target, dst)
        }
    };

    #[cfg(not(windows))]
    let link_res = {
        let _ = meta;
        std::os::unix::fs::symlink(&target, dst)
    };

    match link_res {
        Ok(()) => Ok(CopyOutcome::Copied),
        Err(err)
            if err.kind() == std::io::ErrorKind::PermissionDenied
                || err.raw_os_error() == Some(1314) =>
        {
            // Unprivileged Windows users without Developer Mode cannot create symlinks;
            // safely skip rather than following a potential cycle or failing the whole batch.
            tracing::warn!(
                src = %src.display(),
                dst = %dst.display(),
                "Skipping symlink copy due to missing symlink creation privilege"
            );
            Ok(CopyOutcome::Skipped)
        }
        Err(err) => Err(OnyxError::Io(err)),
    }
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

/// Removes a directory tree safely without ever following symlinks or junctions into external trees.
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

#[allow(clippy::too_many_arguments)]
fn move_items_blocking(
    op_id: u64,
    sources: &[PathBuf],
    destination_dir: &Path,
    conflict: ConflictPolicy,
    mut resolver: Option<Box<dyn ConflictResolver>>,
    cancel: &CancellationToken,
    progress: Option<&mpsc::Sender<OperationProgress>>,
    space_checker: &dyn SpaceChecker,
) -> OnyxResult<OperationReport> {
    if !destination_dir.is_dir() {
        return Err(OnyxError::NotFound {
            path: destination_dir.to_path_buf(),
        });
    }

    ensure_not_recursive(sources, destination_dir)?;

    // Check space for sources that appear to be on a different volume.
    let cross_vol_sources: Vec<PathBuf> = sources
        .iter()
        .filter(|src| !is_same_volume(src, destination_dir))
        .cloned()
        .collect();
    let total_bytes = measure_total_bytes(&cross_vol_sources);
    check_disk_space(space_checker, destination_dir, total_bytes)?;

    let total_items = sources.len();
    let mut report = OperationReport::default();
    let mut reverse_moves: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut transfer = TransferProgress::new(
        op_id,
        OperationKind::Move,
        total_items,
        total_bytes,
        progress,
    );
    let mut sticky_resolution: Option<ConflictResolution> = None;

    for src in sources {
        if cancel.is_cancelled() {
            transfer.emit(&src.to_string_lossy(), true, true);
            return Err(OnyxError::Cancelled);
        }

        let item_name = match src.file_name() {
            Some(name) => name.to_os_string(),
            None => {
                report
                    .failed
                    .push((src.clone(), "Source path has no file name".to_string()));
                transfer.complete(src);
                continue;
            }
        };

        let meta = match fs::symlink_metadata(src) {
            Ok(m) => m,
            Err(err) => {
                report.failed.push((src.clone(), err.to_string()));
                transfer.complete(src);
                continue;
            }
        };

        let desired_target = destination_dir.join(&item_name);
        if is_same_normalized_path(src, &desired_target) {
            report.skipped.push(src.clone());
            transfer.complete(src);
            continue;
        }

        let target = match resolve_item_conflict(
            src,
            &desired_target,
            conflict,
            &mut resolver,
            &mut sticky_resolution,
        ) {
            Ok(PathConflictOutcome::Skip) => {
                report.skipped.push(src.clone());
                transfer.complete(src);
                continue;
            }
            Ok(PathConflictOutcome::Proceed(path)) => path,
            Ok(PathConflictOutcome::Overwrite(path)) => {
                if let Err(err) = remove_existing_for_overwrite(&path) {
                    report.failed.push((src.clone(), err.to_string()));
                    transfer.complete(src);
                    continue;
                }
                path
            }
            Err(OnyxError::Cancelled) => {
                transfer.emit(&src.to_string_lossy(), true, true);
                return Err(OnyxError::Cancelled);
            }
            Err(err) => {
                report.failed.push((src.clone(), err.to_string()));
                transfer.complete(src);
                continue;
            }
        };

        let move_res = match fs::rename(src, &target) {
            Ok(()) => Ok(()),
            Err(err) if is_cross_device_error(&err) => {
                // Cross-volume move: copy completely, then delete source.
                let result = copy_for_cross_volume_move(src, &target, &meta, cancel, &transfer);
                match result {
                    Ok(CopyOutcome::Copied) => {
                        check_cancelled(cancel).and_then(|()| remove_existing_for_overwrite(src))
                    }
                    Ok(CopyOutcome::Skipped) => {
                        report.skipped.push(src.clone());
                        transfer.complete(src);
                        continue;
                    }
                    Err(err) => Err(err),
                }
            }
            Err(err) => Err(OnyxError::Io(err)),
        };

        match move_res {
            Ok(()) => {
                reverse_moves.push((target.clone(), src.clone()));
                report.succeeded.push(target);
            }
            Err(OnyxError::Cancelled) => {
                transfer.emit(&src.to_string_lossy(), true, true);
                return Err(OnyxError::Cancelled);
            }
            Err(err) => {
                report.failed.push((src.clone(), err.to_string()));
            }
        }

        transfer.complete(src);
    }

    if !reverse_moves.is_empty() {
        report.undo_action = Some(UndoAction::ReverseMoves(reverse_moves));
    }

    transfer.emit("", true, false);

    Ok(report)
}

fn copy_for_cross_volume_move(
    src: &Path,
    dst: &Path,
    meta: &fs::Metadata,
    cancel: &CancellationToken,
    progress: &TransferProgress<'_>,
) -> OnyxResult<CopyOutcome> {
    let mut files = Vec::new();
    let mut plan = CopyPlan {
        files: &mut files,
        top_idx: 0,
        cancel,
        require_complete: true,
        visited: HashSet::new(),
    };
    let outcome = plan.entry(src, dst, meta)?;
    let workers = onyx_windows::recommended_transfer_workers(src, dst);
    let mut failures = execute_copy_plan(&files, 1, workers, cancel, progress)?;
    match failures.pop().flatten() {
        Some(err) => Err(err),
        None => Ok(outcome),
    }
}

fn is_cross_device_error(err: &std::io::Error) -> bool {
    // Win32 ERROR_NOT_SAME_DEVICE = 17
    err.kind() == std::io::ErrorKind::CrossesDevices || err.raw_os_error() == Some(17)
}

fn rename_item_blocking(
    source: &Path,
    new_name: &str,
    conflict: ConflictPolicy,
) -> OnyxResult<OperationReport> {
    if !path_exists_any(source) {
        return Err(OnyxError::NotFound {
            path: source.to_path_buf(),
        });
    }

    let parent = source.parent().ok_or_else(|| OnyxError::InvalidPath {
        message: format!("Cannot rename root path '{}'", source.display()),
    })?;
    let desired = parent.join(new_name);

    let mut report = OperationReport::default();
    let target = match resolve_conflict(source, &desired, conflict, true)? {
        PathConflictOutcome::Skip => {
            report.skipped.push(source.to_path_buf());
            return Ok(report);
        }
        PathConflictOutcome::Proceed(path) => path,
        PathConflictOutcome::Overwrite(path) => {
            remove_existing_for_overwrite(&path)?;
            path
        }
    };

    fs::rename(source, &target)?;
    report.succeeded.push(target.clone());
    report.undo_action = Some(UndoAction::ReverseRename {
        from: target,
        to: source.to_path_buf(),
    });
    Ok(report)
}

fn create_dir_blocking(parent_dir: &Path, name: &str) -> OnyxResult<OperationReport> {
    if !parent_dir.is_dir() {
        return Err(OnyxError::NotFound {
            path: parent_dir.to_path_buf(),
        });
    }

    let target = parent_dir.join(name);
    if path_exists_any(&target) {
        return Err(OnyxError::AlreadyExists { path: target });
    }

    fs::create_dir(&target)?;
    let mut report = OperationReport::default();
    report.succeeded.push(target.clone());
    report.undo_action = Some(UndoAction::RemoveCreatedDir(target));
    Ok(report)
}

fn recycle_items_blocking(
    op_id: u64,
    paths: &[PathBuf],
    cancel: &CancellationToken,
    progress: Option<&mpsc::Sender<OperationProgress>>,
    recycler: &dyn Recycler,
) -> OnyxResult<OperationReport> {
    let total_items = paths.len();
    let mut report = OperationReport::default();
    let mut completed_items = 0usize;

    for path in paths {
        if cancel.is_cancelled() {
            send_progress(
                progress,
                OperationProgress {
                    op_id,
                    kind: OperationKind::Recycle,
                    current_item: path.display().to_string(),
                    completed_items,
                    total_items,
                    bytes_copied: 0,
                    total_bytes: 0,
                    finished: true,
                    cancelled: true,
                    error: None,
                },
            );
            return Err(OnyxError::Cancelled);
        }

        if !path_exists_any(path) {
            report.failed.push((
                path.clone(),
                OnyxError::NotFound { path: path.clone() }.to_string(),
            ));
            completed_items += 1;
            continue;
        }

        match recycler.recycle(std::slice::from_ref(path)) {
            Ok(()) => report.succeeded.push(path.clone()),
            Err(err) => report.failed.push((path.clone(), err.to_string())),
        }

        completed_items += 1;
        send_progress(
            progress,
            OperationProgress {
                op_id,
                kind: OperationKind::Recycle,
                current_item: path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string()),
                completed_items,
                total_items,
                bytes_copied: 0,
                total_bytes: 0,
                finished: false,
                cancelled: false,
                error: None,
            },
        );
    }

    send_progress(
        progress,
        OperationProgress {
            op_id,
            kind: OperationKind::Recycle,
            current_item: String::new(),
            completed_items,
            total_items,
            bytes_copied: 0,
            total_bytes: 0,
            finished: true,
            cancelled: false,
            error: None,
        },
    );

    Ok(report)
}

fn permanent_delete_blocking(
    op_id: u64,
    paths: &[PathBuf],
    cancel: &CancellationToken,
    progress: Option<&mpsc::Sender<OperationProgress>>,
) -> OnyxResult<OperationReport> {
    let total_items = paths.len();
    let mut report = OperationReport::default();
    let mut completed_items = 0usize;

    for path in paths {
        if cancel.is_cancelled() {
            send_progress(
                progress,
                OperationProgress {
                    op_id,
                    kind: OperationKind::PermanentDelete,
                    current_item: path.display().to_string(),
                    completed_items,
                    total_items,
                    bytes_copied: 0,
                    total_bytes: 0,
                    finished: true,
                    cancelled: true,
                    error: None,
                },
            );
            return Err(OnyxError::Cancelled);
        }

        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(err) => {
                report.failed.push((path.clone(), err.to_string()));
                completed_items += 1;
                continue;
            }
        };

        let del_res = if is_reparse_or_symlink(&meta) {
            if is_dir_reparse(&meta) {
                fs::remove_dir(path).map_err(OnyxError::Io)
            } else {
                fs::remove_file(path).map_err(OnyxError::Io)
            }
        } else if meta.is_dir() {
            remove_dir_safe_no_follow(path)
        } else {
            fs::remove_file(path).map_err(OnyxError::Io)
        };

        match del_res {
            Ok(()) => report.succeeded.push(path.clone()),
            Err(err) => report.failed.push((path.clone(), err.to_string())),
        }

        completed_items += 1;
        send_progress(
            progress,
            OperationProgress {
                op_id,
                kind: OperationKind::PermanentDelete,
                current_item: path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string()),
                completed_items,
                total_items,
                bytes_copied: 0,
                total_bytes: 0,
                finished: false,
                cancelled: false,
                error: None,
            },
        );
    }

    send_progress(
        progress,
        OperationProgress {
            op_id,
            kind: OperationKind::PermanentDelete,
            current_item: String::new(),
            completed_items,
            total_items,
            bytes_copied: 0,
            total_bytes: 0,
            finished: true,
            cancelled: false,
            error: None,
        },
    );

    Ok(report)
}

fn execute_undo_blocking(
    action: UndoAction,
    recycler: &dyn Recycler,
) -> OnyxResult<OperationReport> {
    let mut report = OperationReport::default();

    match action {
        UndoAction::ReverseRename { from, to } => {
            if !path_exists_any(&from) {
                return Err(OnyxError::NotFound { path: from });
            }
            let target = if path_exists_any(&to) && !is_same_normalized_path(&from, &to) {
                generate_auto_rename_path(&to)?
            } else {
                to
            };
            fs::rename(&from, &target)?;
            report.succeeded.push(target);
        }
        UndoAction::RemoveCreatedDir(dir) => {
            if !path_exists_any(&dir) {
                return Err(OnyxError::NotFound { path: dir });
            }
            // Intentionally uses `remove_dir` (not `remove_dir_all`) so undo never destroys
            // files added by the user after the directory was created.
            fs::remove_dir(&dir)?;
            report.succeeded.push(dir);
        }
        UndoAction::ReverseMoves(moves) => {
            for (from, to) in moves.into_iter().rev() {
                if !path_exists_any(&from) {
                    report
                        .failed
                        .push((from.clone(), OnyxError::NotFound { path: from }.to_string()));
                    continue;
                }
                let target = if path_exists_any(&to) && !is_same_normalized_path(&from, &to) {
                    match generate_auto_rename_path(&to) {
                        Ok(p) => p,
                        Err(err) => {
                            report.failed.push((from, err.to_string()));
                            continue;
                        }
                    }
                } else {
                    to
                };
                match fs::rename(&from, &target) {
                    Ok(()) => report.succeeded.push(target),
                    Err(err) => report.failed.push((from, err.to_string())),
                }
            }
        }
        UndoAction::RecycleCreatedCopies(paths) => {
            for path in paths {
                if !path_exists_any(&path) {
                    report.skipped.push(path);
                    continue;
                }
                match recycler.recycle(std::slice::from_ref(&path)) {
                    Ok(()) => report.succeeded.push(path),
                    Err(err) => report.failed.push((path, err.to_string())),
                }
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    #[tokio::test]
    async fn copies_64_small_files_and_preserves_modified_time() {
        let work = TempDir::new().unwrap();
        let (engine, _) = test_engine(&work);
        let source = work.path().join("collection");
        let destination = work.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        let modified = std::time::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        for index in 0..64u8 {
            let path = source.join(format!("file-{index}.bin"));
            let size = if index % 2 == 0 { 8 * 1024 } else { 96 * 1024 };
            fs::write(&path, vec![index; size]).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(modified)
                .unwrap();
        }

        let report = engine
            .copy_items(
                std::slice::from_ref(&source),
                &destination,
                ConflictPolicy::Skip,
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        let copied = destination.join("collection");
        assert_eq!(report.succeeded, std::slice::from_ref(&copied));
        assert!(report.failed.is_empty());
        assert!(engine.can_undo().await);

        // Force the parallel executor as well so that this test covers it on an HDD.
        let forced = work.path().join("forced-parallel");
        let mut files = Vec::new();
        let cancel = CancellationToken::new();
        let mut plan = CopyPlan {
            files: &mut files,
            top_idx: 0,
            cancel: &cancel,
            require_complete: false,
            visited: HashSet::new(),
        };
        plan.entry(&source, &forced, &fs::metadata(&source).unwrap())
            .unwrap();
        let progress = TransferProgress::new(1, OperationKind::Copy, 1, 0, None);
        let failures = execute_copy_plan(&files, 1, 4, &cancel, &progress).unwrap();
        assert!(failures[0].is_none());

        for index in 0..64u8 {
            let name = format!("file-{index}.bin");
            for path in [copied.join(&name), forced.join(name)] {
                let size = if index % 2 == 0 { 8 * 1024 } else { 96 * 1024 };
                assert_eq!(fs::read(&path).unwrap(), vec![index; size]);
                assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
            }
        }
    }

    #[tokio::test]
    async fn large_copy_reports_bytes_and_honors_cancellation() {
        let work = TempDir::new().unwrap();
        let (engine, _) = test_engine(&work);
        let source = work.path().join("large.bin");
        let destination = work.path().join("destination");
        fs::create_dir(&destination).unwrap();
        let contents = vec![0xba; 16 * 1024 * 1024 + 7];
        fs::write(&source, &contents).unwrap();
        let (tx, mut rx) = mpsc::channel(32);
        let report = engine
            .copy_items(
                std::slice::from_ref(&source),
                &destination,
                ConflictPolicy::Skip,
                CancellationToken::new(),
                Some(tx),
            )
            .await
            .unwrap();

        let copied = destination.join("large.bin");
        assert_eq!(fs::read(&copied).unwrap(), contents);
        assert_eq!(report.succeeded, [copied]);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        assert!(
            events
                .iter()
                .any(|event| !event.finished && event.bytes_copied > 0)
        );
        assert!(
            events
                .windows(2)
                .all(|events| events[0].bytes_copied <= events[1].bytes_copied)
        );
        let finished = events.last().unwrap();
        assert!(finished.finished);
        assert_eq!(finished.bytes_copied, contents.len() as u64);
        assert_eq!(finished.completed_items, 1);

        let cancelled_destination = work.path().join("cancelled");
        fs::create_dir(&cancelled_destination).unwrap();
        let (tx, mut rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = engine
            .copy_items(
                &[source],
                &cancelled_destination,
                ConflictPolicy::Skip,
                cancel,
                Some(tx),
            )
            .await;
        assert!(matches!(result, Err(OnyxError::Cancelled)));
        assert!(rx.recv().await.unwrap().cancelled);
        assert_eq!(fs::read_dir(cancelled_destination).unwrap().count(), 0);
    }

    #[test]
    fn cancels_a_large_transfer_after_progress_and_removes_the_partial_file() {
        let root = std::env::current_dir().unwrap();
        let work = tempfile::tempdir_in(&root).unwrap();
        // A relative source forces the native copy path even on a ReFS test volume.
        let source = work.path().strip_prefix(&root).unwrap().join("source.bin");
        let destination = work.path().join("destination.bin");
        let size = 16 * 1024 * 1024 + 7;
        File::create(&source).unwrap().set_len(size).unwrap();
        let file = PlannedFile {
            src: source,
            dst: destination.clone(),
            size,
            top_idx: 0,
        };
        let (tx, _rx) = mpsc::channel(32);
        let progress = TransferProgress::new(1, OperationKind::Copy, 1, size, Some(&tx));
        let cancel = CancellationToken::new();

        std::thread::scope(|scope| {
            // Hold the progress lock until the callback accounts for its first bytes.
            let update = progress.last_update.lock().unwrap();
            let worker = scope.spawn(|| execute_copy_plan(&[file], 1, 1, &cancel, &progress));
            let start = Instant::now();
            while progress.bytes.load(Ordering::Relaxed) == 0
                && !worker.is_finished()
                && start.elapsed() < Duration::from_secs(5)
            {
                std::thread::yield_now();
            }
            cancel.cancel();
            drop(update);
            assert!(matches!(worker.join().unwrap(), Err(OnyxError::Cancelled)));
        });

        assert!(progress.bytes.load(Ordering::Relaxed) > 0);
        assert!(!destination.exists());
    }

    #[tokio::test]
    async fn pending_top_level_file_participates_in_conflict_resolution() {
        let work = TempDir::new().unwrap();
        let (engine, _) = test_engine(&work);
        let first_dir = work.path().join("first");
        let second_dir = work.path().join("second");
        let destination = work.path().join("destination");
        for path in [&first_dir, &second_dir, &destination] {
            fs::create_dir(path).unwrap();
        }
        let first = first_dir.join("file.txt");
        let second = second_dir.join("file.txt");
        fs::write(&first, b"first").unwrap();
        fs::write(&second, b"second").unwrap();

        let report = engine
            .copy_items(
                &[first, second.clone()],
                &destination,
                ConflictPolicy::Skip,
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        let copied = destination.join("file.txt");
        assert_eq!(fs::read(&copied).unwrap(), b"first");
        assert_eq!(report.succeeded, [copied]);
        assert_eq!(report.skipped, [second]);
    }

    #[test]
    fn failed_parallel_member_marks_its_top_level_item_failed() {
        let work = TempDir::new().unwrap();
        let source = work.path().join("source");
        let destination = work.path().join("destination");
        fs::create_dir(&source).unwrap();
        for index in 0..4 {
            fs::write(source.join(format!("file-{index}.bin")), b"some data").unwrap();
        }

        let mut batch = CopyBatch::default();
        let cancel = CancellationToken::new();
        let mut plan = CopyPlan {
            files: &mut batch.files,
            top_idx: 0,
            cancel: &cancel,
            require_complete: false,
            visited: HashSet::new(),
        };
        plan.entry(&source, &destination, &fs::metadata(&source).unwrap())
            .unwrap();
        batch.items.push((&source, Ok(Some(destination.clone()))));
        fs::remove_file(source.join("file-0.bin")).unwrap();

        let mut report = OperationReport::default();
        let mut progress = TransferProgress::new(1, OperationKind::Copy, 1, 0, None);
        batch
            .execute(4, &cancel, &mut progress, &mut report)
            .unwrap();
        assert!(report.succeeded.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, source);
    }

    /// Mock recycler that moves files into an isolated temporary trash folder instead of
    /// touching the real Windows Recycle Bin.
    #[derive(Debug)]
    struct MockRecycler {
        trash_dir: PathBuf,
        recycled: StdMutex<Vec<PathBuf>>,
        fail_on: StdMutex<Option<PathBuf>>,
    }

    impl MockRecycler {
        fn new(trash_dir: PathBuf) -> Self {
            Self {
                trash_dir,
                recycled: StdMutex::new(Vec::new()),
                fail_on: StdMutex::new(None),
            }
        }

        fn set_fail_on(&self, path: PathBuf) {
            *self.fail_on.lock().unwrap() = Some(path);
        }

        fn recycled_paths(&self) -> Vec<PathBuf> {
            self.recycled.lock().unwrap().clone()
        }
    }

    impl Recycler for MockRecycler {
        fn recycle(&self, paths: &[PathBuf]) -> OnyxResult<()> {
            for path in paths {
                if self
                    .fail_on
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|p| p == path)
                {
                    return Err(OnyxError::PermissionDenied { path: path.clone() });
                }
                let name = path.file_name().unwrap().to_os_string();
                let mut dst = self.trash_dir.join(&name);
                if dst.exists() {
                    dst = generate_auto_rename_path(&dst)?;
                }
                fs::rename(path, &dst)?;
                self.recycled.lock().unwrap().push(path.clone());
            }
            Ok(())
        }
    }

    /// Mock space checker for deterministic `InsufficientSpace` fault-injection tests.
    #[derive(Debug)]
    struct FixedSpaceChecker(Option<u64>);

    impl SpaceChecker for FixedSpaceChecker {
        fn free_space(&self, _path: &Path) -> Option<u64> {
            self.0
        }
    }

    fn test_engine(trash: &TempDir) -> (FileOperationsEngine, Arc<MockRecycler>) {
        let recycler = Arc::new(MockRecycler::new(trash.path().to_path_buf()));
        let engine =
            FileOperationsEngine::with_recycler(Arc::clone(&recycler) as Arc<dyn Recycler>);
        (engine, recycler)
    }

    #[tokio::test]
    async fn copy_move_rename_create_dir_duplicate_and_undo() {
        let work = TempDir::new().unwrap();
        let trash = TempDir::new().unwrap();
        let (engine, recycler) = test_engine(&trash);

        // 1. Create directory + undo
        let create_rep = engine.create_dir(work.path(), "Alpha").await.unwrap();
        let alpha_dir = work.path().join("Alpha");
        assert!(alpha_dir.is_dir());
        assert_eq!(create_rep.succeeded, vec![alpha_dir.clone()]);
        assert!(engine.can_undo().await);

        let undo_create = engine.undo_last().await.unwrap().unwrap();
        assert!(!alpha_dir.exists());
        assert_eq!(undo_create.succeeded, vec![alpha_dir.clone()]);

        // Recreate Alpha and Beta
        engine.create_dir(work.path(), "Alpha").await.unwrap();
        engine.create_dir(work.path(), "Beta").await.unwrap();
        let beta_dir = work.path().join("Beta");

        // 2. Copy file with progress streaming
        let src_file = alpha_dir.join("doc.txt");
        fs::write(&src_file, b"hello onyx world").unwrap();
        let (tx, mut rx) = mpsc::channel(32);

        let copy_rep = engine
            .copy_items(
                std::slice::from_ref(&src_file),
                &beta_dir,
                ConflictPolicy::Fail,
                CancellationToken::new(),
                Some(tx),
            )
            .await
            .unwrap();

        let copied_file = beta_dir.join("doc.txt");
        assert!(copied_file.exists());
        assert_eq!(fs::read(&copied_file).unwrap(), b"hello onyx world");
        assert_eq!(copy_rep.succeeded, vec![copied_file.clone()]);

        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        assert!(events.last().is_some_and(|e| e.finished && !e.cancelled));

        // Undo copy -> recycles the copy via MockRecycler (not real Recycle Bin)
        let _ = engine.undo_last().await.unwrap().unwrap();
        assert!(!copied_file.exists());
        assert!(recycler.recycled_paths().contains(&copied_file));

        // 3. Duplicate file -> produces "doc (2).txt"
        let dup_rep = engine
            .duplicate_item(&src_file, CancellationToken::new(), None)
            .await
            .unwrap();
        let dup_file = alpha_dir.join("doc (2).txt");
        assert!(dup_file.exists());
        assert_eq!(dup_rep.succeeded, vec![dup_file.clone()]);

        // 4. Rename file + undo
        let rename_rep = engine
            .rename_item(&dup_file, "renamed.txt", ConflictPolicy::Fail)
            .await
            .unwrap();
        let renamed_file = alpha_dir.join("renamed.txt");
        assert!(renamed_file.exists());
        assert!(!dup_file.exists());
        assert_eq!(rename_rep.succeeded, vec![renamed_file.clone()]);

        let _ = engine.undo_last().await.unwrap().unwrap();
        assert!(dup_file.exists());
        assert!(!renamed_file.exists());

        // 5. Move file + undo
        let move_rep = engine
            .move_items(
                std::slice::from_ref(&dup_file),
                &beta_dir,
                ConflictPolicy::Fail,
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        let moved_file = beta_dir.join("doc (2).txt");
        assert!(moved_file.exists());
        assert!(!dup_file.exists());
        assert_eq!(move_rep.succeeded, vec![moved_file.clone()]);

        let _ = engine.undo_last().await.unwrap().unwrap();
        assert!(dup_file.exists());
        assert!(!moved_file.exists());
    }

    #[tokio::test]
    async fn conflict_policies_skip_overwrite_autorename_fail() {
        let work = TempDir::new().unwrap();
        let trash = TempDir::new().unwrap();
        let (engine, _) = test_engine(&trash);

        let src_dir = work.path().join("src");
        let dst_dir = work.path().join("dst");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&dst_dir).unwrap();

        let src = src_dir.join("data.txt");
        let existing = dst_dir.join("data.txt");
        fs::write(&src, b"new-content").unwrap();
        fs::write(&existing, b"old-content").unwrap();

        // Skip
        let skip_rep = engine
            .copy_items(
                std::slice::from_ref(&src),
                &dst_dir,
                ConflictPolicy::Skip,
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(skip_rep.skipped, vec![src.clone()]);
        assert_eq!(fs::read(&existing).unwrap(), b"old-content");

        // Fail (collected into report.failed)
        let fail_rep = engine
            .copy_items(
                std::slice::from_ref(&src),
                &dst_dir,
                ConflictPolicy::Fail,
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(fail_rep.failed.len(), 1);
        assert_eq!(fs::read(&existing).unwrap(), b"old-content");

        // AutoRename -> "data (2).txt"
        let auto_rep = engine
            .copy_items(
                std::slice::from_ref(&src),
                &dst_dir,
                ConflictPolicy::AutoRename,
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(auto_rep.succeeded, vec![dst_dir.join("data (2).txt")]);
        assert_eq!(
            fs::read(dst_dir.join("data (2).txt")).unwrap(),
            b"new-content"
        );

        // Overwrite
        let over_rep = engine
            .copy_items(
                std::slice::from_ref(&src),
                &dst_dir,
                ConflictPolicy::Overwrite,
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(over_rep.succeeded, vec![existing.clone()]);
        assert_eq!(fs::read(&existing).unwrap(), b"new-content");
    }

    #[tokio::test]
    async fn recursion_guard_refuses_copy_and_move_into_self_or_child() {
        let work = TempDir::new().unwrap();
        let trash = TempDir::new().unwrap();
        let (engine, _) = test_engine(&trash);

        let folder = work.path().join("Folder");
        let nested = folder.join("Nested");
        fs::create_dir_all(&nested).unwrap();

        let copy_self = engine
            .copy_items(
                std::slice::from_ref(&folder),
                &folder,
                ConflictPolicy::AutoRename,
                CancellationToken::new(),
                None,
            )
            .await;
        assert!(matches!(
            copy_self,
            Err(OnyxError::RecursiveOperation { .. })
        ));

        let copy_child = engine
            .copy_items(
                std::slice::from_ref(&folder),
                &nested,
                ConflictPolicy::AutoRename,
                CancellationToken::new(),
                None,
            )
            .await;
        assert!(matches!(
            copy_child,
            Err(OnyxError::RecursiveOperation { .. })
        ));

        let move_child = engine
            .move_items(
                std::slice::from_ref(&folder),
                &nested,
                ConflictPolicy::AutoRename,
                CancellationToken::new(),
                None,
            )
            .await;
        assert!(matches!(
            move_child,
            Err(OnyxError::RecursiveOperation { .. })
        ));
    }

    #[tokio::test]
    async fn permanent_delete_requires_explicit_confirmation() {
        let work = TempDir::new().unwrap();
        let trash = TempDir::new().unwrap();
        let (engine, _) = test_engine(&trash);

        let file = work.path().join("secret.txt");
        fs::write(&file, b"important").unwrap();

        let refused = engine
            .permanent_delete(
                PermanentDeleteRequest::unconfirmed(vec![file.clone()]),
                CancellationToken::new(),
                None,
            )
            .await;
        assert!(refused.is_err());
        assert!(
            file.exists(),
            "Unconfirmed permanent delete must never delete files"
        );

        let deleted = engine
            .permanent_delete(
                PermanentDeleteRequest::confirmed(vec![file.clone()]),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(deleted.succeeded, vec![file.clone()]);
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn fault_injection_insufficient_disk_space() {
        let work = TempDir::new().unwrap();
        let trash = TempDir::new().unwrap();
        let recycler = Arc::new(MockRecycler::new(trash.path().to_path_buf()));
        let tiny_disk = Arc::new(FixedSpaceChecker(Some(10))); // only 10 bytes free
        let engine = FileOperationsEngine::with_providers(recycler, tiny_disk);

        let src_dir = work.path().join("src");
        let dst_dir = work.path().join("dst");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&dst_dir).unwrap();

        let big_file = src_dir.join("payload.bin");
        fs::write(&big_file, vec![0xAB; 1024]).unwrap();

        let result = engine
            .copy_items(
                std::slice::from_ref(&big_file),
                &dst_dir,
                ConflictPolicy::Fail,
                CancellationToken::new(),
                None,
            )
            .await;

        assert!(matches!(
            result,
            Err(OnyxError::InsufficientSpace {
                required: 1024,
                available: 10
            })
        ));
        assert!(!dst_dir.join("payload.bin").exists());
    }

    #[tokio::test]
    async fn fault_injection_cancellation_and_partial_failure_collection() {
        let work = TempDir::new().unwrap();
        let trash = TempDir::new().unwrap();
        let (engine, recycler) = test_engine(&trash);

        // Pre-cancelled token immediately cancels copy
        let f1 = work.path().join("a.txt");
        let dst_dir = work.path().join("dst");
        fs::create_dir_all(&dst_dir).unwrap();
        fs::write(&f1, b"abc").unwrap();

        let cancel = CancellationToken::new();
        cancel.cancel();
        let cancelled = engine
            .copy_items(
                std::slice::from_ref(&f1),
                &dst_dir,
                ConflictPolicy::AutoRename,
                cancel,
                None,
            )
            .await;
        assert!(matches!(cancelled, Err(OnyxError::Cancelled)));

        // Partial failure collection during recycle (one succeeds, one fails via injected error, one missing)
        let f2 = work.path().join("b.txt");
        let missing = work.path().join("missing.txt");
        fs::write(&f2, b"def").unwrap();
        recycler.set_fail_on(f2.clone());

        let rep = engine
            .recycle_items(
                &[f1.clone(), f2.clone(), missing.clone()],
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(rep.succeeded, vec![f1.clone()]);
        assert_eq!(rep.failed.len(), 2);
        assert!(!f1.exists());
        assert!(f2.exists());
    }

    #[tokio::test]
    async fn undo_remove_created_dir_refuses_non_empty_directory() {
        let work = TempDir::new().unwrap();
        let trash = TempDir::new().unwrap();
        let (engine, _) = test_engine(&trash);

        engine.create_dir(work.path(), "UserFolder").await.unwrap();
        let created = work.path().join("UserFolder");
        fs::write(created.join("precious.txt"), b"keep me").unwrap();

        let undo_res = engine.undo_last().await;
        assert!(
            undo_res.is_err(),
            "Undo must refuse to delete a non-empty created dir"
        );
        assert!(created.join("precious.txt").exists());
    }

    #[tokio::test]
    async fn case_only_rename_succeeds_without_conflict() {
        let work = TempDir::new().unwrap();
        let trash = TempDir::new().unwrap();
        let (engine, _) = test_engine(&trash);

        let lower = work.path().join("readme.txt");
        fs::write(&lower, b"hello").unwrap();

        let rep = engine
            .rename_item(&lower, "README.txt", ConflictPolicy::Fail)
            .await
            .unwrap();
        assert_eq!(rep.succeeded, vec![work.path().join("README.txt")]);
        assert_eq!(fs::read(work.path().join("README.txt")).unwrap(), b"hello");
    }

    #[tokio::test]
    async fn recursive_directory_copy_preserves_nested_tree() {
        let work = TempDir::new().unwrap();
        let trash = TempDir::new().unwrap();
        let (engine, _) = test_engine(&trash);

        let src_tree = work.path().join("Tree");
        let dst_root = work.path().join("Dest");
        fs::create_dir_all(src_tree.join("sub").join("deep")).unwrap();
        fs::create_dir_all(&dst_root).unwrap();
        fs::write(src_tree.join("root.txt"), b"r").unwrap();
        fs::write(src_tree.join("sub").join("deep").join("leaf.txt"), b"leaf").unwrap();

        let rep = engine
            .copy_items(
                std::slice::from_ref(&src_tree),
                &dst_root,
                ConflictPolicy::Fail,
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(rep.succeeded, vec![dst_root.join("Tree")]);
        assert_eq!(
            fs::read(
                dst_root
                    .join("Tree")
                    .join("sub")
                    .join("deep")
                    .join("leaf.txt")
            )
            .unwrap(),
            b"leaf"
        );
    }

    #[tokio::test]
    async fn junction_cycle_does_not_recurse_infinitely_on_copy_or_delete() {
        let work = TempDir::new().unwrap();
        let trash = TempDir::new().unwrap();
        let (engine, _) = test_engine(&trash);

        let tree = work.path().join("CycleRoot");
        let dst = work.path().join("Dest");
        fs::create_dir_all(&tree).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(tree.join("hello.txt"), b"ok").unwrap();

        // Create a self-referential NTFS directory junction (`CycleRoot\loop -> CycleRoot`)
        // using `cmd /C mklink /J`, which works without Administrator elevation on NTFS.
        let loop_junction = tree.join("loop");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&loop_junction)
            .arg(&tree)
            .output();

        if let Ok(out) = status
            && out.status.success()
        {
            // Copying `CycleRoot` must terminate cleanly and copy `hello.txt` without infinite recursion.
            let rep = engine
                .copy_items(
                    std::slice::from_ref(&tree),
                    &dst,
                    ConflictPolicy::Fail,
                    CancellationToken::new(),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(rep.succeeded, vec![dst.join("CycleRoot")]);
            assert_eq!(
                fs::read(dst.join("CycleRoot").join("hello.txt")).unwrap(),
                b"ok"
            );

            // Permanent delete must unlink the junction safely without looping or escaping.
            let del = engine
                .permanent_delete(
                    PermanentDeleteRequest::confirmed(vec![tree.clone()]),
                    CancellationToken::new(),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(del.succeeded, vec![tree.clone()]);
            assert!(!tree.exists());
        }
    }

    #[tokio::test]
    async fn copy_with_resolver_handles_overwrite_skip_keep_both_and_cancel() {
        use crate::path_guard::generate_keep_both_path;
        use crate::types::ConflictDecision;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        let work = tempfile::TempDir::new().unwrap();
        let trash = tempfile::TempDir::new().unwrap();
        let (engine, _) = test_engine(&trash);

        let src_dir = work.path().join("src");
        let dst_dir = work.path().join("dst");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&dst_dir).unwrap();

        // Prepare colliding files: overwrite, skip, keep-both (with existing (2) so it jumps to (3)),
        // and extensionless file.
        let f_over = src_dir.join("overwrite.txt");
        let f_skip = src_dir.join("skip.txt");
        let f_keep = src_dir.join("report.pdf");
        let f_noext = src_dir.join("README");
        fs::write(&f_over, b"new-over").unwrap();
        fs::write(&f_skip, b"new-skip").unwrap();
        fs::write(&f_keep, b"new-pdf").unwrap();
        fs::write(&f_noext, b"new-readme").unwrap();

        fs::write(dst_dir.join("overwrite.txt"), b"old-over").unwrap();
        fs::write(dst_dir.join("skip.txt"), b"old-skip").unwrap();
        fs::write(dst_dir.join("report.pdf"), b"old-pdf-1").unwrap();
        fs::write(dst_dir.join("report (2).pdf"), b"old-pdf-2").unwrap();
        fs::write(dst_dir.join("README"), b"old-readme").unwrap();

        assert_eq!(
            generate_keep_both_path(&dst_dir.join("report.pdf")).unwrap(),
            dst_dir.join("report (3).pdf")
        );
        assert_eq!(
            generate_keep_both_path(&dst_dir.join("README")).unwrap(),
            dst_dir.join("README (2)")
        );

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let report = engine
            .copy_items_with_resolver(
                &[f_over, f_skip.clone(), f_keep, f_noext],
                &dst_dir,
                ConflictPolicy::Fail,
                move |info: &ConflictInfo| {
                    calls_clone.fetch_add(1, AtomicOrdering::SeqCst);
                    assert!(!info.is_dir);
                    match info
                        .destination
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                    {
                        "overwrite.txt" => ConflictDecision::once(ConflictResolution::Overwrite),
                        "skip.txt" => ConflictDecision::once(ConflictResolution::Skip),
                        _ => ConflictDecision::once(ConflictResolution::KeepBoth),
                    }
                },
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(calls.load(AtomicOrdering::SeqCst), 4);
        assert_eq!(report.skipped, vec![f_skip]);
        assert_eq!(
            fs::read(dst_dir.join("overwrite.txt")).unwrap(),
            b"new-over"
        );
        assert_eq!(fs::read(dst_dir.join("skip.txt")).unwrap(), b"old-skip");
        assert_eq!(fs::read(dst_dir.join("report.pdf")).unwrap(), b"old-pdf-1");
        assert_eq!(
            fs::read(dst_dir.join("report (2).pdf")).unwrap(),
            b"old-pdf-2"
        );
        assert_eq!(
            fs::read(dst_dir.join("report (3).pdf")).unwrap(),
            b"new-pdf"
        );
        assert_eq!(fs::read(dst_dir.join("README (2)")).unwrap(), b"new-readme");

        // Cancel decision aborts operation cleanly.
        let f_cancel = src_dir.join("cancel.txt");
        let f_after = src_dir.join("after.txt");
        fs::write(&f_cancel, b"c").unwrap();
        fs::write(&f_after, b"a").unwrap();
        fs::write(dst_dir.join("cancel.txt"), b"existing").unwrap();

        let err = engine
            .copy_items_with_resolver(
                &[f_cancel, f_after],
                &dst_dir,
                ConflictPolicy::Fail,
                |_: &ConflictInfo| ConflictDecision::once(ConflictResolution::Cancel),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, OnyxError::Cancelled));
        assert!(!dst_dir.join("after.txt").exists());
    }

    #[tokio::test]
    async fn resolver_apply_to_all_reuses_decision_across_copy_and_move() {
        use crate::types::ConflictDecision;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        let work = tempfile::TempDir::new().unwrap();
        let trash = tempfile::TempDir::new().unwrap();
        let (engine, _) = test_engine(&trash);

        let src_dir = work.path().join("src");
        let dst_dir = work.path().join("dst");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&dst_dir).unwrap();

        let mut sources = Vec::new();
        for name in ["a.txt", "b.txt", "c.txt"] {
            let s = src_dir.join(name);
            fs::write(&s, format!("new-{name}")).unwrap();
            fs::write(dst_dir.join(name), format!("old-{name}")).unwrap();
            sources.push(s);
        }

        let copy_calls = Arc::new(AtomicUsize::new(0));
        let copy_calls_clone = Arc::clone(&copy_calls);
        let rep = engine
            .copy_items_with_resolver(
                &sources,
                &dst_dir,
                ConflictPolicy::Fail,
                move |_: &ConflictInfo| {
                    copy_calls_clone.fetch_add(1, AtomicOrdering::SeqCst);
                    ConflictDecision::for_all(ConflictResolution::KeepBoth)
                },
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(copy_calls.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(rep.succeeded.len(), 3);
        assert!(dst_dir.join("a (2).txt").exists());
        assert!(dst_dir.join("b (2).txt").exists());
        assert!(dst_dir.join("c (2).txt").exists());

        // Move with apply_to_all = Skip invokes resolver only once and leaves all sources intact.
        let move_calls = Arc::new(AtomicUsize::new(0));
        let move_calls_clone = Arc::clone(&move_calls);
        let move_rep = engine
            .move_items_with_resolver(
                &sources,
                &dst_dir,
                ConflictPolicy::Fail,
                move |_: &ConflictInfo| {
                    move_calls_clone.fetch_add(1, AtomicOrdering::SeqCst);
                    ConflictDecision::for_all(ConflictResolution::Skip)
                },
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(move_calls.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(move_rep.skipped.len(), 3);
        for s in &sources {
            assert!(s.exists());
        }
    }

    #[tokio::test]
    async fn source_equals_destination_overwrite_preserves_data() {
        use crate::types::ConflictDecision;

        let work = tempfile::TempDir::new().unwrap();
        let trash = tempfile::TempDir::new().unwrap();
        let (engine, _) = test_engine(&trash);

        let file = work.path().join("important.txt");
        fs::write(&file, b"precious-data").unwrap();

        let copy_rep = engine
            .copy_items_with_resolver(
                std::slice::from_ref(&file),
                work.path(),
                ConflictPolicy::Fail,
                |_: &ConflictInfo| ConflictDecision::once(ConflictResolution::Overwrite),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(copy_rep.skipped, vec![file.clone()]);
        assert_eq!(fs::read(&file).unwrap(), b"precious-data");

        let move_rep = engine
            .move_items_with_resolver(
                std::slice::from_ref(&file),
                work.path(),
                ConflictPolicy::Fail,
                |_: &ConflictInfo| ConflictDecision::once(ConflictResolution::Overwrite),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(move_rep.skipped, vec![file.clone()]);
        assert_eq!(fs::read(&file).unwrap(), b"precious-data");
    }
}
