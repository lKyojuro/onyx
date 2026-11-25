use onyx_core::{FavoriteItem, ThemeMode, ViewMode};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Maximum number of recent locations retained in [`AppConfig::recent_locations`].
pub const MAX_RECENT_LOCATIONS: usize = 20;

/// Default terminal profile identifier (`"wt"` = Windows Terminal).
pub const DEFAULT_TERMINAL: &str = "wt";

/// Supported terminal profile identifiers for [`AppConfig::default_terminal`].
pub const SUPPORTED_TERMINALS: &[&str] = &["wt", "pwsh", "cmd"];

/// Persistent user configuration for Onyx.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub theme: ThemeMode,
    pub accent_hex: Option<String>,
    pub show_hidden: bool,
    pub show_system: bool,
    pub default_view: ViewMode,
    pub dual_pane_default: bool,
    pub show_preview_pane: bool,
    #[serde(default = "default_terminal_value")]
    pub default_terminal: String,
    pub favorites: Vec<FavoriteItem>,
    pub excluded_index_dirs: Vec<PathBuf>,
    pub recent_locations: Vec<PathBuf>,
}

fn default_terminal_value() -> String {
    DEFAULT_TERMINAL.to_string()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            theme: ThemeMode::default(),
            accent_hex: None,
            show_hidden: false,
            show_system: false,
            default_view: ViewMode::default(),
            dual_pane_default: false,
            show_preview_pane: true,
            default_terminal: default_terminal_value(),
            favorites: Vec::new(),
            excluded_index_dirs: Vec::new(),
            recent_locations: Vec::new(),
        }
    }
}

impl AppConfig {
    /// Returns the normalized terminal profile identifier (`"wt"`, `"pwsh"`, or `"cmd"`).
    #[must_use]
    pub fn normalized_terminal(&self) -> &str {
        match self.default_terminal.trim().to_ascii_lowercase().as_str() {
            "pwsh" => "pwsh",
            "cmd" => "cmd",
            _ => DEFAULT_TERMINAL,
        }
    }

    /// Sets the default terminal profile if recognized, falling back to `"wt"`.
    pub fn set_default_terminal(&mut self, value: &str) {
        self.default_terminal = match value.trim().to_ascii_lowercase().as_str() {
            "pwsh" | "powershell" => "pwsh".to_string(),
            "cmd" | "command-prompt" => "cmd".to_string(),
            _ => DEFAULT_TERMINAL.to_string(),
        };
    }

    /// Adds a directory to `excluded_index_dirs` if not already present.
    /// Returns `true` if a new entry was inserted.
    pub fn add_excluded_index_dir(&mut self, path: PathBuf) -> bool {
        if path.as_os_str().is_empty()
            || self
                .excluded_index_dirs
                .iter()
                .any(|existing| paths_equal(existing, &path))
        {
            return false;
        }
        self.excluded_index_dirs.push(path);
        true
    }

    /// Removes all entries matching `path` from `excluded_index_dirs`.
    /// Returns `true` if at least one entry was removed.
    pub fn remove_excluded_index_dir(&mut self, path: &Path) -> bool {
        let before = self.excluded_index_dirs.len();
        self.excluded_index_dirs
            .retain(|existing| !paths_equal(existing, path));
        self.excluded_index_dirs.len() != before
    }

    /// Adds a favorite directory, updating the display name if the path already exists.
    pub fn add_favorite(&mut self, item: FavoriteItem) {
        if let Some(existing) = self
            .favorites
            .iter_mut()
            .find(|fav| paths_equal(&fav.path, &item.path))
        {
            existing.name = item.name;
            existing.path = item.path;
        } else {
            self.favorites.push(item);
        }
    }

    /// Removes all favorites matching `path`. Returns `true` if at least one entry was removed.
    pub fn remove_favorite(&mut self, path: &Path) -> bool {
        let before = self.favorites.len();
        self.favorites.retain(|fav| !paths_equal(&fav.path, path));
        self.favorites.len() != before
    }

    /// Records a visited location at the front of `recent_locations`, deduplicating
    /// and bounding the list to [`MAX_RECENT_LOCATIONS`] (20) entries.
    pub fn record_recent(&mut self, path: PathBuf) {
        self.recent_locations
            .retain(|existing| !paths_equal(existing, &path));
        self.recent_locations.insert(0, path);
        self.recent_locations.truncate(MAX_RECENT_LOCATIONS);
    }
}

/// Persistent window and tab session state restored across restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionState {
    pub tabs: Vec<PathBuf>,
    pub active_tab: usize,
    pub dual_pane: bool,
    pub secondary_path: Option<PathBuf>,
    pub window_width: u32,
    pub window_height: u32,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            active_tab: 0,
            dual_pane: false,
            secondary_path: None,
            window_width: 1280,
            window_height: 800,
        }
    }
}

fn normalize_for_cmp(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let trimmed = if raw.len() > 3 {
        raw.trim_end_matches(['\\', '/'])
    } else {
        raw.as_ref()
    };
    #[cfg(windows)]
    {
        trimmed.replace('/', "\\").to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        trimmed.to_string()
    }
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    normalize_for_cmp(a) == normalize_for_cmp(b)
}
