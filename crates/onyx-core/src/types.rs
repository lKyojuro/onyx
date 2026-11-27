use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Monotonically increasing generation token preventing stale async results from overwriting current state.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct GenerationId(pub u64);

impl GenerationId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }

    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Identifies a file pane in single- or dual-pane mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum PaneId {
    #[default]
    Primary,
    Secondary,
}

/// File entry classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum FileKind {
    #[default]
    File,
    Directory,
    Symlink,
    ReparsePoint,
    Other,
}

impl FileKind {
    #[must_use]
    pub const fn is_dir(self) -> bool {
        matches!(self, Self::Directory)
    }
}

/// Windows-compatible file attribute flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FileAttributes {
    pub hidden: bool,
    pub system: bool,
    pub readonly: bool,
    pub archive: bool,
    pub compressed: bool,
    pub encrypted: bool,
    pub reparse_point: bool,
}

/// Core representation of a filesystem item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: PathBuf,
    pub name: String,
    pub extension: Option<String>,
    pub kind: FileKind,
    pub size: u64,
    /// Unix timestamp in seconds (UTC).
    pub modified: Option<i64>,
    /// Unix timestamp in seconds (UTC).
    pub created: Option<i64>,
    pub attributes: FileAttributes,
    pub is_symlink: bool,
    pub symlink_target: Option<PathBuf>,
    pub type_label: String,
}

impl FileEntry {
    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.kind.is_dir()
    }
}

/// Storage drive classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum DriveKind {
    Fixed,
    Removable,
    Network,
    CdRom,
    RamDisk,
    #[default]
    Unknown,
}

/// Metadata about a mounted drive or volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveInfo {
    pub root: PathBuf,
    pub label: String,
    pub filesystem: String,
    pub kind: DriveKind,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

impl DriveInfo {
    #[must_use]
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.free_bytes)
    }

    #[must_use]
    pub fn usage_ratio(&self) -> f32 {
        if self.total_bytes == 0 {
            0.0
        } else {
            (self.used_bytes() as f64 / self.total_bytes as f64).clamp(0.0, 1.0) as f32
        }
    }
}

/// Standard Windows known folders shown in the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KnownFolderKind {
    Home,
    Desktop,
    Downloads,
    Documents,
    Pictures,
    Music,
    Videos,
}

/// Resolved known folder location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownFolder {
    pub kind: KnownFolderKind,
    pub name: String,
    pub path: PathBuf,
}

/// User-pinned favorite directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FavoriteItem {
    pub name: String,
    pub path: PathBuf,
}

/// File list view layout mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ViewMode {
    #[default]
    Details,
    List,
    Grid,
}

/// Application visual theme mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ThemeMode {
    #[default]
    Dark,
    Oled,
    Light,
    System,
}
