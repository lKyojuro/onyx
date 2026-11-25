#![forbid(unsafe_code)]
#![cfg_attr(windows, windows_subsystem = "windows")]

mod commands;
mod content;
mod controller;
mod logging;
mod navigation;
mod operations;
mod state;
mod ui_sync;

use onyx_config::{AppConfig, ConfigStore};
use onyx_core::{GenerationId, to_extended_length_path};
use onyx_fs::{EnumerationOptions, FsEngine};
use onyx_preview::{PreviewContent, PreviewEngine};
use onyx_search::{LiveSearchEngine, SearchQuery};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::runtime::Builder;
use tokio_util::sync::CancellationToken;

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

enum Launch {
    Version,
    Smoke,
    Window(Option<PathBuf>),
}

fn launch_mode() -> AppResult<Launch> {
    let mut args = std::env::args_os().skip(1);
    let Some(arg) = args.next() else {
        return Ok(Launch::Window(None));
    };
    let mode = match arg.to_str() {
        Some("--version" | "-V") => Launch::Version,
        Some("--smoke-test") => Launch::Smoke,
        Some("--") => Launch::Window(Some(args.next().ok_or("missing path after --")?.into())),
        Some(flag) if flag.starts_with('-') => return Err(format!("unknown option: {flag}").into()),
        _ => Launch::Window(Some(arg.into())),
    };
    if args.next().is_some() {
        return Err("usage: onyx [PATH] | --version | --smoke-test".into());
    }
    Ok(mode)
}

fn main() -> AppResult<()> {
    onyx_windows::attach_parent_console();
    let mode = launch_mode()?;
    if matches!(mode, Launch::Version) {
        println!("onyx {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let _logging = logging::init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "Onyx started");
    let result = run(mode);
    if let Err(error) = &result {
        tracing::error!(%error, "Onyx exited with an error");
    }
    result
}

fn run(mode: Launch) -> AppResult<()> {
    configure_runtime_parallelism()?;
    let runtime = Arc::new(
        Builder::new_multi_thread()
            .worker_threads(async_worker_threads())
            .max_blocking_threads(MAX_BLOCKING_THREADS)
            .enable_all()
            .build()?,
    );
    match mode {
        Launch::Version => Ok(()),
        Launch::Smoke => runtime.block_on(smoke_test()),
        Launch::Window(path) => controller::run(runtime, path),
    }
}

/// Lower bound for Tokio async workers. A file manager's event loop does not benefit
/// from more.
const MIN_ASYNC_WORKERS: usize = 2;

/// Upper bound for Tokio async workers, chosen for low idle RAM instead of pegging
/// every logical CPU (Tokio defaults to one worker per logical core).
const MAX_ASYNC_WORKERS: usize = 4;

/// Bound for Tokio's on-demand blocking pool (default 512). 32 covers the folder-size
/// scanning semaphore, thumbnails, and archive/operation workers with headroom.
const MAX_BLOCKING_THREADS: usize = 32;

/// Bound for the Rayon global pool used by parallel directory-entry sorting (default:
/// one thread per logical core).
const MAX_RAYON_THREADS: usize = 4;

fn async_worker_threads() -> usize {
    clamp_worker_threads(
        std::thread::available_parallelism().map_or(MIN_ASYNC_WORKERS, |n| n.get()),
    )
}

fn clamp_worker_threads(parallelism: usize) -> usize {
    parallelism.clamp(MIN_ASYNC_WORKERS, MAX_ASYNC_WORKERS)
}

fn configure_runtime_parallelism() -> AppResult<()> {
    // The Rayon global pool must be sized before the first `par_*` call (entry sorting in
    // `onyx-fs`), so configure it as the very first startup step.
    if let Err(error) = rayon::ThreadPoolBuilder::new()
        .num_threads(MAX_RAYON_THREADS)
        .build_global()
    {
        tracing::warn!(%error, "Rayon global pool already initialized; using its size");
    }
    Ok(())
}

struct SmokeDir(PathBuf);

impl SmokeDir {
    fn new() -> std::io::Result<Self> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let path = std::env::temp_dir().join(format!("onyx-smoke-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for SmokeDir {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            tracing::warn!(%error, "could not remove smoke-test directory");
        }
    }
}

async fn smoke_test() -> AppResult<()> {
    let (dir, drives, folders, usn) = tokio::task::spawn_blocking(|| -> AppResult<_> {
        let temporary = SmokeDir::new()?;
        let store = ConfigStore::with_dir(temporary.0.clone());
        let config = store.load_config();
        if config != AppConfig::default() {
            return Err("smoke test did not load the default configuration".into());
        }
        store.save_config(&config)?;
        if store.load_config() != config {
            return Err("smoke test configuration did not round-trip".into());
        }
        let dir = std::env::current_dir()?;
        let drives = onyx_windows::enumerate_drives().len();
        let folders = onyx_windows::enumerate_known_folders().len();
        let usn = onyx_windows::probe_ntfs_usn_capability(&dir);
        Ok((dir, drives, folders, usn))
    })
    .await??;

    let cancel = CancellationToken::new();
    let entries = FsEngine::new()
        .enumerate_dir(
            dir.clone(),
            GenerationId::new(1),
            EnumerationOptions::default(),
            cancel.clone(),
        )
        .await?;
    let manifest = dir.join("Cargo.toml");
    let file = entries
        .iter()
        .find(|entry| entry.path == manifest && !entry.is_dir())
        .or_else(|| entries.iter().find(|entry| !entry.is_dir()));
    let query = SearchQuery {
        text: file.map_or_else(|| "Cargo.toml".into(), |entry| entry.name.clone()),
        max_results: 5,
        use_index: false,
        ..SearchQuery::default()
    };
    let results = LiveSearchEngine::new()
        .search(dir.clone(), query, cancel.clone())
        .await?;
    if file.is_some() && results.is_empty() {
        return Err("smoke test search returned no results for a listed file".into());
    }
    let path = file.map_or(dir, |entry| entry.path.clone());
    let preview = PreviewEngine::new().load_preview(path, cancel).await?;
    let kind = match preview {
        PreviewContent::Text { .. } => "text",
        PreviewContent::Image { .. } => "image",
        PreviewContent::DocumentMetadata { .. } => "document",
        PreviewContent::DirectorySummary { .. } => "directory",
        PreviewContent::BinaryMetadata { .. } => "binary",
        PreviewContent::Unavailable { reason } => return Err(reason.into()),
    };
    println!(
        "smoke-test: ok | config=default drives={drives} known_folders={folders} entries={} search_matches={} preview={kind} usn={usn:?}",
        entries.len(),
        results.len()
    );
    Ok(())
}

fn is_directory(path: &Path) -> bool {
    to_extended_length_path(path).is_dir()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn worker_thread_count_is_clamped_to_desktop_range() {
        assert_eq!(clamp_worker_threads(1), MIN_ASYNC_WORKERS);
        assert_eq!(clamp_worker_threads(MIN_ASYNC_WORKERS), MIN_ASYNC_WORKERS);
        assert_eq!(clamp_worker_threads(3), 3);
        assert_eq!(clamp_worker_threads(MAX_ASYNC_WORKERS), MAX_ASYNC_WORKERS);
        assert_eq!(clamp_worker_threads(16), MAX_ASYNC_WORKERS);
        assert!(clamp_worker_threads(usize::MAX) <= MAX_ASYNC_WORKERS);
    }

    #[test]
    fn async_worker_threads_stays_within_bounds_on_any_host() {
        let workers = async_worker_threads();
        assert!(
            (MIN_ASYNC_WORKERS..=MAX_ASYNC_WORKERS).contains(&workers),
            "async worker count {workers} outside desktop bounds"
        );
    }
}
