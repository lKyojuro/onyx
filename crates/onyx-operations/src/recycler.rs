//! Platform abstraction traits for Recycle Bin deletion and free disk space queries,
//! enabling deterministic, side-effect-free unit and fault-injection tests.

use onyx_core::OnyxResult;
use std::fmt::Debug;
use std::path::{Path, PathBuf};

/// Abstraction over Recycle Bin deletion so tests never pollute the user's real Recycle Bin.
pub trait Recycler: Send + Sync + Debug {
    /// Sends `paths` to the Recycle Bin.
    fn recycle(&self, paths: &[PathBuf]) -> OnyxResult<()>;
}

/// Default production [`Recycler`] delegating to `onyx_windows::recycle_paths` (`IFileOperation`).
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsRecycler;

impl Recycler for WindowsRecycler {
    fn recycle(&self, paths: &[PathBuf]) -> OnyxResult<()> {
        onyx_windows::recycle_paths(paths)
    }
}

/// Abstraction over volume free-space queries for disk-space preflight checks.
pub trait SpaceChecker: Send + Sync + Debug {
    /// Returns available free bytes on the volume hosting `path`, or `None` if unavailable.
    fn free_space(&self, path: &Path) -> Option<u64>;
}

/// Default production [`SpaceChecker`] delegating to `onyx_windows::free_disk_space`.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsSpaceChecker;

impl SpaceChecker for WindowsSpaceChecker {
    fn free_space(&self, path: &Path) -> Option<u64> {
        onyx_windows::free_disk_space(path)
    }
}
