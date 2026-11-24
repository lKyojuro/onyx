use onyx_core::{FavoriteItem, ThemeMode, ViewMode};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Maximum number of recent locations retained in [`AppConfig::recent_locations`].
pub const MAX_RECENT_LOCATIONS: usize = 20;

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
    pub favorites: Vec<FavoriteItem>,
    pub excluded_index_dirs: Vec<PathBuf>,
    pub recent_locations: Vec<PathBuf>,
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
            favorites: Vec::new(),
            excluded_index_dirs: Vec::new(),
            recent_locations: Vec::new(),
        }
    }
}

impl AppConfig {
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
