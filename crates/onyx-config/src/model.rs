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

/// Default logical width of the sidebar in pixels.
pub const DEFAULT_SIDEBAR_WIDTH: f32 = 226.0;
/// Minimum logical width of the sidebar in pixels.
pub const MIN_SIDEBAR_WIDTH: f32 = 180.0;
/// Maximum logical width of the sidebar in pixels.
pub const MAX_SIDEBAR_WIDTH: f32 = 400.0;

/// Default logical width of the preview pane in single-pane mode.
pub const DEFAULT_PREVIEW_WIDTH_SINGLE: f32 = 310.0;
/// Default logical width of the preview pane in dual-pane mode.
pub const DEFAULT_PREVIEW_WIDTH_DUAL: f32 = 264.0;
/// Minimum logical width of the preview pane in pixels.
pub const MIN_PREVIEW_WIDTH: f32 = 220.0;
/// Maximum logical width of the preview pane in pixels.
pub const MAX_PREVIEW_WIDTH: f32 = 480.0;

/// Default split ratio between the primary and secondary file panes.
pub const DEFAULT_PANE_SPLIT_RATIO: f32 = 0.5;
/// Minimum split ratio for the primary file pane.
pub const MIN_PANE_SPLIT_RATIO: f32 = 0.15;
/// Maximum split ratio for the primary file pane.
pub const MAX_PANE_SPLIT_RATIO: f32 = 0.85;

/// Stable sidebar group identifier for Favorites.
pub const SIDEBAR_GROUP_FAVORITES: &str = "favorites";
/// Stable sidebar group identifier for Quick Access.
pub const SIDEBAR_GROUP_QUICK_ACCESS: &str = "quick_access";
/// Stable sidebar group identifier for Drives.
pub const SIDEBAR_GROUP_DRIVES: &str = "drives";
/// All valid sidebar group identifiers.
pub const VALID_SIDEBAR_GROUPS: &[&str] = &[
    SIDEBAR_GROUP_FAVORITES,
    SIDEBAR_GROUP_QUICK_ACCESS,
    SIDEBAR_GROUP_DRIVES,
];

/// Dock side for the preview pane relative to the file panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewSide {
    Left,
    #[default]
    Right,
}

impl PreviewSide {
    /// Returns the opposite dock side.
    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

/// Clamps a sidebar width to `[MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH]`, falling back to
/// [`DEFAULT_SIDEBAR_WIDTH`] if non-finite.
#[must_use]
pub fn clamp_sidebar_width(width: f32) -> f32 {
    if width.is_finite() {
        width.clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH)
    } else {
        DEFAULT_SIDEBAR_WIDTH
    }
}

/// Clamps an optional preview width to `[MIN_PREVIEW_WIDTH, MAX_PREVIEW_WIDTH]`, dropping
/// non-finite values.
#[must_use]
pub fn clamp_preview_width(width: Option<f32>) -> Option<f32> {
    width
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(MIN_PREVIEW_WIDTH, MAX_PREVIEW_WIDTH))
}

/// Clamps a dual-pane split ratio to `[MIN_PANE_SPLIT_RATIO, MAX_PANE_SPLIT_RATIO]`,
/// falling back to [`DEFAULT_PANE_SPLIT_RATIO`] if non-finite.
#[must_use]
pub fn clamp_pane_split_ratio(ratio: f32) -> f32 {
    if ratio.is_finite() {
        ratio.clamp(MIN_PANE_SPLIT_RATIO, MAX_PANE_SPLIT_RATIO)
    } else {
        DEFAULT_PANE_SPLIT_RATIO
    }
}

/// Filters `groups` to known [`VALID_SIDEBAR_GROUPS`] identifiers, deduplicating in order.
#[must_use]
pub fn sanitize_collapsed_sidebar_groups(groups: Vec<String>) -> Vec<String> {
    let mut sanitized = Vec::with_capacity(groups.len());
    for group in groups {
        if VALID_SIDEBAR_GROUPS.contains(&group.as_str()) && !sanitized.contains(&group) {
            sanitized.push(group);
        }
    }
    sanitized
}

/// Persistent window and tab session state restored across restarts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionState {
    pub tabs: Vec<PathBuf>,
    pub active_tab: usize,
    pub dual_pane: bool,
    pub secondary_path: Option<PathBuf>,
    pub window_width: u32,
    pub window_height: u32,
    pub sidebar_visible: bool,
    pub sidebar_width: f32,
    pub preview_width: Option<f32>,
    pub pane_split_ratio: f32,
    pub collapsed_sidebar_groups: Vec<String>,
    pub preview_side: PreviewSide,
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
            sidebar_visible: true,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            preview_width: None,
            pane_split_ratio: DEFAULT_PANE_SPLIT_RATIO,
            collapsed_sidebar_groups: Vec::new(),
            preview_side: PreviewSide::Right,
        }
    }
}

impl SessionState {
    /// Clamps and sanitizes persisted layout fields to safe bounds.
    pub fn sanitize(&mut self) {
        self.sidebar_width = clamp_sidebar_width(self.sidebar_width);
        self.preview_width = clamp_preview_width(self.preview_width);
        self.pane_split_ratio = clamp_pane_split_ratio(self.pane_split_ratio);
        self.collapsed_sidebar_groups =
            sanitize_collapsed_sidebar_groups(std::mem::take(&mut self.collapsed_sidebar_groups));
    }

    /// Resets layout preferences to defaults while preserving tabs, active tab,
    /// dual-pane mode, secondary path, and window dimensions.
    pub fn reset_layout(&mut self) {
        self.sidebar_visible = true;
        self.sidebar_width = DEFAULT_SIDEBAR_WIDTH;
        self.preview_width = None;
        self.pane_split_ratio = DEFAULT_PANE_SPLIT_RATIO;
        self.collapsed_sidebar_groups.clear();
        self.preview_side = PreviewSide::Right;
    }

    /// Returns the preferred preview width, falling back to the single- or dual-pane default.
    #[must_use]
    pub fn effective_preview_width(&self) -> f32 {
        self.preview_width.unwrap_or(if self.dual_pane {
            DEFAULT_PREVIEW_WIDTH_DUAL
        } else {
            DEFAULT_PREVIEW_WIDTH_SINGLE
        })
    }

    /// Returns `true` if the given sidebar group ID is currently collapsed.
    #[must_use]
    pub fn is_sidebar_group_collapsed(&self, group_id: &str) -> bool {
        self.collapsed_sidebar_groups
            .iter()
            .any(|id| id == group_id)
    }

    /// Sets the collapsed state for a valid sidebar group ID.
    pub fn set_sidebar_group_collapsed(&mut self, group_id: &str, collapsed: bool) {
        if !VALID_SIDEBAR_GROUPS.contains(&group_id) {
            return;
        }
        if collapsed {
            if !self.is_sidebar_group_collapsed(group_id) {
                self.collapsed_sidebar_groups.push(group_id.to_string());
            }
        } else {
            self.collapsed_sidebar_groups.retain(|id| id != group_id);
        }
    }

    /// Toggles the collapsed state for a valid sidebar group ID and returns the new state.
    pub fn toggle_sidebar_group(&mut self, group_id: &str) -> bool {
        let next = !self.is_sidebar_group_collapsed(group_id);
        self.set_sidebar_group_collapsed(group_id, next);
        self.is_sidebar_group_collapsed(group_id)
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
