//! Persistent atomic configuration and session recovery for Onyx.

mod model;
mod store;

pub use model::{
    AppConfig, DEFAULT_PANE_SPLIT_RATIO, DEFAULT_PREVIEW_WIDTH_DUAL, DEFAULT_PREVIEW_WIDTH_SINGLE,
    DEFAULT_SIDEBAR_WIDTH, DEFAULT_TERMINAL, MAX_PANE_SPLIT_RATIO, MAX_PREVIEW_WIDTH,
    MAX_RECENT_LOCATIONS, MAX_SIDEBAR_WIDTH, MIN_PANE_SPLIT_RATIO, MIN_PREVIEW_WIDTH,
    MIN_SIDEBAR_WIDTH, PreviewSide, SIDEBAR_GROUP_DRIVES, SIDEBAR_GROUP_FAVORITES,
    SIDEBAR_GROUP_QUICK_ACCESS, SUPPORTED_TERMINALS, SessionState, VALID_SIDEBAR_GROUPS,
    clamp_pane_split_ratio, clamp_preview_width, clamp_sidebar_width,
    sanitize_collapsed_sidebar_groups,
};
pub use store::{ConfigStore, atomic_write};
