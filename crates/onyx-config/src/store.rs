use crate::model::{AppConfig, SessionState};
use onyx_core::{OnyxError, OnyxResult};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::warn;

const CONFIG_FILE_NAME: &str = "config.toml";
const SESSION_FILE_NAME: &str = "session.json";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Manages reading and atomically persisting [`AppConfig`] and [`SessionState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigStore {
    dir: PathBuf,
}

impl ConfigStore {
    /// Returns a store backed by the platform user configuration directory (`<config_dir>/onyx`).
    #[must_use]
    pub fn default_location() -> Self {
        let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        Self {
            dir: base.join("onyx"),
        }
    }

    /// Returns a store rooted at a custom directory (useful for isolated tests).
    #[must_use]
    pub fn with_dir(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Returns the directory containing config and session files.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Full path to `config.toml`.
    #[must_use]
    pub fn config_path(&self) -> PathBuf {
        self.dir.join(CONFIG_FILE_NAME)
    }

    /// Full path to `session.json`.
    #[must_use]
    pub fn session_path(&self) -> PathBuf {
        self.dir.join(SESSION_FILE_NAME)
    }

    /// Loads `AppConfig` from disk, falling back to [`AppConfig::default`] if missing or corrupt.
    #[must_use]
    pub fn load_config(&self) -> AppConfig {
        let path = self.config_path();
        let contents = match fs::read_to_string(&path) {
            Ok(data) => data,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return AppConfig::default(),
            Err(err) => {
                warn!(path = %path.display(), error = %err, "Failed to read config file; using defaults");
                return AppConfig::default();
            }
        };

        match toml::from_str::<AppConfig>(&contents) {
            Ok(cfg) => cfg,
            Err(err) => {
                warn!(path = %path.display(), error = %err, "Corrupt config TOML; falling back to defaults");
                AppConfig::default()
            }
        }
    }

    /// Atomically persists `AppConfig` as TOML.
    pub fn save_config(&self, config: &AppConfig) -> OnyxResult<()> {
        let serialized = toml::to_string_pretty(config).map_err(|err| OnyxError::Config {
            message: format!("failed to serialize AppConfig: {err}"),
        })?;
        atomic_write(&self.config_path(), serialized.as_bytes())
    }

    /// Loads `SessionState` from disk, falling back to [`SessionState::default`] if missing or corrupt.
    #[must_use]
    pub fn load_session(&self) -> SessionState {
        let path = self.session_path();
        let contents = match fs::read_to_string(&path) {
            Ok(data) => data,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return SessionState::default();
            }
            Err(err) => {
                warn!(path = %path.display(), error = %err, "Failed to read session file; using defaults");
                return SessionState::default();
            }
        };

        match serde_json::from_str::<SessionState>(&contents) {
            Ok(mut session) => {
                session.sanitize();
                session
            }
            Err(err) => {
                warn!(path = %path.display(), error = %err, "Corrupt session JSON; falling back to defaults");
                SessionState::default()
            }
        }
    }

    /// Atomically persists `SessionState` as JSON.
    pub fn save_session(&self, session: &SessionState) -> OnyxResult<()> {
        let serialized = serde_json::to_vec_pretty(session).map_err(|err| OnyxError::Config {
            message: format!("failed to serialize SessionState: {err}"),
        })?;
        atomic_write(&self.session_path(), &serialized)
    }
}

/// Atomically writes `data` to `target` by writing to `<target>.tmp.<pid>` in the same directory,
/// flushing to disk with `sync_all()`, and atomically replacing `target`.
pub fn atomic_write(target: &Path, data: &[u8]) -> OnyxResult<()> {
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let pid = std::process::id();
    let (tmp_path, mut file) = loop {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = append_suffix(target, &format!(".tmp.{pid}.{sequence}"));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => break (path, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    };

    let write_res = file.write_all(data).and_then(|()| file.sync_all());
    drop(file);

    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(OnyxError::Io(err));
    }

    if let Err(error) = fs::rename(&tmp_path, target) {
        let _ = fs::remove_file(&tmp_path);
        return Err(OnyxError::Io(error));
    }

    Ok(())
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::model::MAX_RECENT_LOCATIONS;
    use onyx_core::{FavoriteItem, ThemeMode, ViewMode};
    use tempfile::TempDir;

    #[test]
    fn config_round_trip_and_atomic_overwrite() {
        let tmp = TempDir::new().unwrap();
        let store = ConfigStore::with_dir(tmp.path().to_path_buf());

        // Missing config returns default.
        assert_eq!(store.load_config(), AppConfig::default());

        let mut cfg = AppConfig {
            theme: ThemeMode::Light,
            accent_hex: Some("#0078D4".to_string()),
            show_hidden: true,
            show_system: false,
            default_view: ViewMode::Grid,
            dual_pane_default: true,
            show_preview_pane: false,
            calculate_folder_sizes: false,
            default_terminal: "pwsh".to_string(),
            favorites: vec![FavoriteItem {
                name: "Projects".to_string(),
                path: PathBuf::from(r"D:\Projects"),
            }],
            excluded_index_dirs: vec![PathBuf::from(r"C:\Windows")],
            indexed_search: true,
            recent_locations: vec![PathBuf::from(r"D:\Onyx")],
        };

        store.save_config(&cfg).unwrap();
        assert_eq!(store.load_config(), cfg);

        // Overwrite existing file cleanly without leaving temp files behind.
        cfg.theme = ThemeMode::System;
        cfg.accent_hex = Some("#FF5500".to_string());
        cfg.default_terminal = "cmd".to_string();
        store.save_config(&cfg).unwrap();
        assert_eq!(store.load_config(), cfg);

        let left_over_tmp = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().contains(".tmp."));
        assert!(!left_over_tmp);
    }

    #[test]
    fn corrupt_config_and_session_fallback_to_default() {
        let tmp = TempDir::new().unwrap();
        let store = ConfigStore::with_dir(tmp.path().to_path_buf());

        fs::write(store.config_path(), b"not = [valid toml").unwrap();
        assert_eq!(store.load_config(), AppConfig::default());

        fs::write(store.session_path(), b"{corrupt json").unwrap();
        assert_eq!(store.load_session(), SessionState::default());
    }

    #[test]
    fn session_round_trip() {
        let tmp = TempDir::new().unwrap();
        let store = ConfigStore::with_dir(tmp.path().to_path_buf());

        let session = SessionState {
            tabs: vec![PathBuf::from(r"C:\Users"), PathBuf::from(r"D:\Onyx")],
            active_tab: 1,
            dual_pane: true,
            secondary_path: Some(PathBuf::from(r"D:\Projects")),
            window_width: 1920,
            window_height: 1080,
            sidebar_visible: false,
            sidebar_width: 280.0,
            preview_width: Some(340.0),
            pane_split_ratio: 0.62,
            collapsed_sidebar_groups: vec![
                crate::model::SIDEBAR_GROUP_FAVORITES.to_string(),
                crate::model::SIDEBAR_GROUP_DRIVES.to_string(),
            ],
            preview_side: crate::model::PreviewSide::Left,
        };

        store.save_session(&session).unwrap();
        assert_eq!(store.load_session(), session);
    }

    #[test]
    fn legacy_session_missing_layout_fields_uses_defaults() {
        let tmp = TempDir::new().unwrap();
        let store = ConfigStore::with_dir(tmp.path().to_path_buf());

        let legacy_json = r#"{
            "tabs": ["C:\\Users", "D:\\Onyx"],
            "active_tab": 1,
            "dual_pane": true,
            "secondary_path": "D:\\Projects",
            "window_width": 1600,
            "window_height": 900
        }"#;
        fs::write(store.session_path(), legacy_json).unwrap();

        let loaded = store.load_session();
        assert_eq!(
            loaded.tabs,
            vec![PathBuf::from(r"C:\Users"), PathBuf::from(r"D:\Onyx")]
        );
        assert_eq!(loaded.active_tab, 1);
        assert!(loaded.dual_pane);
        assert_eq!(loaded.secondary_path, Some(PathBuf::from(r"D:\Projects")));
        assert_eq!(loaded.window_width, 1600);
        assert_eq!(loaded.window_height, 900);
        assert!(loaded.sidebar_visible);
        assert_eq!(loaded.sidebar_width, crate::model::DEFAULT_SIDEBAR_WIDTH);
        assert_eq!(loaded.preview_width, None);
        assert_eq!(
            loaded.effective_preview_width(),
            crate::model::DEFAULT_PREVIEW_WIDTH_DUAL
        );
        assert_eq!(
            loaded.pane_split_ratio,
            crate::model::DEFAULT_PANE_SPLIT_RATIO
        );
        assert!(loaded.collapsed_sidebar_groups.is_empty());
        assert_eq!(loaded.preview_side, crate::model::PreviewSide::Right);
    }

    #[test]
    fn session_load_clamps_and_sanitizes_layout_values() {
        let tmp = TempDir::new().unwrap();
        let store = ConfigStore::with_dir(tmp.path().to_path_buf());

        let out_of_bounds_json = r#"{
            "tabs": ["C:\\Users"],
            "active_tab": 0,
            "dual_pane": false,
            "secondary_path": null,
            "window_width": 1280,
            "window_height": 800,
            "sidebar_visible": false,
            "sidebar_width": 999.0,
            "preview_width": 50.0,
            "pane_split_ratio": 0.99,
            "collapsed_sidebar_groups": ["favorites", "unknown_group", "favorites", "drives"],
            "preview_side": "left"
        }"#;
        fs::write(store.session_path(), out_of_bounds_json).unwrap();

        let loaded = store.load_session();
        assert!(!loaded.sidebar_visible);
        assert_eq!(loaded.sidebar_width, crate::model::MAX_SIDEBAR_WIDTH);
        assert_eq!(loaded.preview_width, Some(crate::model::MIN_PREVIEW_WIDTH));
        assert_eq!(loaded.pane_split_ratio, crate::model::MAX_PANE_SPLIT_RATIO);
        assert_eq!(
            loaded.collapsed_sidebar_groups,
            vec!["favorites".to_string(), "drives".to_string()]
        );
        assert_eq!(loaded.preview_side, crate::model::PreviewSide::Left);

        let mut non_finite = SessionState {
            sidebar_width: f32::NAN,
            preview_width: Some(f32::INFINITY),
            pane_split_ratio: f32::NEG_INFINITY,
            ..SessionState::default()
        };
        non_finite.sanitize();
        assert_eq!(
            non_finite.sidebar_width,
            crate::model::DEFAULT_SIDEBAR_WIDTH
        );
        assert_eq!(non_finite.preview_width, None);
        assert_eq!(
            non_finite.effective_preview_width(),
            crate::model::DEFAULT_PREVIEW_WIDTH_SINGLE
        );
        assert_eq!(
            non_finite.pane_split_ratio,
            crate::model::DEFAULT_PANE_SPLIT_RATIO
        );
    }

    #[test]
    fn session_reset_layout_preserves_tabs_and_paths() {
        let mut session = SessionState {
            tabs: vec![PathBuf::from(r"C:\Users"), PathBuf::from(r"D:\Onyx")],
            active_tab: 1,
            dual_pane: true,
            secondary_path: Some(PathBuf::from(r"D:\Projects")),
            window_width: 1440,
            window_height: 900,
            sidebar_visible: false,
            sidebar_width: 350.0,
            preview_width: Some(420.0),
            pane_split_ratio: 0.7,
            collapsed_sidebar_groups: vec!["quick_access".to_string()],
            preview_side: crate::model::PreviewSide::Left,
        };

        session.reset_layout();

        assert_eq!(
            session.tabs,
            vec![PathBuf::from(r"C:\Users"), PathBuf::from(r"D:\Onyx")]
        );
        assert_eq!(session.active_tab, 1);
        assert!(session.dual_pane);
        assert_eq!(session.secondary_path, Some(PathBuf::from(r"D:\Projects")));
        assert_eq!(session.window_width, 1440);
        assert_eq!(session.window_height, 900);
        assert!(session.sidebar_visible);
        assert_eq!(session.sidebar_width, crate::model::DEFAULT_SIDEBAR_WIDTH);
        assert_eq!(session.preview_width, None);
        assert_eq!(
            session.pane_split_ratio,
            crate::model::DEFAULT_PANE_SPLIT_RATIO
        );
        assert!(session.collapsed_sidebar_groups.is_empty());
        assert_eq!(session.preview_side, crate::model::PreviewSide::Right);
    }

    #[test]
    fn favorites_and_recent_helpers() {
        let mut cfg = AppConfig::default();

        cfg.add_favorite(FavoriteItem {
            name: "Docs".to_string(),
            path: PathBuf::from(r"C:\Docs"),
        });
        cfg.add_favorite(FavoriteItem {
            name: "Documents".to_string(),
            path: PathBuf::from(r"c:\docs\"),
        });
        assert_eq!(cfg.favorites.len(), 1);
        assert_eq!(cfg.favorites[0].name, "Documents");

        assert!(cfg.remove_favorite(Path::new(r"C:\Docs")));
        assert!(!cfg.remove_favorite(Path::new(r"C:\Docs")));
        assert!(cfg.favorites.is_empty());

        for i in 0..25 {
            cfg.record_recent(PathBuf::from(format!(r"C:\Dir{i}")));
        }
        assert_eq!(cfg.recent_locations.len(), MAX_RECENT_LOCATIONS);
        assert_eq!(cfg.recent_locations[0], PathBuf::from(r"C:\Dir24"));

        // Recording an already-present location moves it to the front without growing length.
        cfg.record_recent(PathBuf::from(r"C:\Dir10"));
        assert_eq!(cfg.recent_locations.len(), MAX_RECENT_LOCATIONS);
        assert_eq!(cfg.recent_locations[0], PathBuf::from(r"C:\Dir10"));
    }

    #[test]
    fn legacy_config_without_default_terminal_loads_with_default_wt() {
        let tmp = TempDir::new().unwrap();
        let store = ConfigStore::with_dir(tmp.path().to_path_buf());

        let legacy_toml = r#"
theme = "Dark"
show_hidden = true
show_system = false
default_view = "Grid"
dual_pane_default = false
show_preview_pane = true
excluded_index_dirs = ['C:\Windows']
"#;
        fs::write(store.config_path(), legacy_toml).unwrap();

        let loaded = store.load_config();
        assert_eq!(loaded.default_terminal, "wt");
        assert_eq!(loaded.normalized_terminal(), "wt");
        assert!(loaded.show_hidden);
        assert!(loaded.calculate_folder_sizes);
        assert_eq!(loaded.default_view, ViewMode::Grid);
        assert_eq!(
            loaded.excluded_index_dirs,
            vec![PathBuf::from(r"C:\Windows")]
        );
    }

    #[test]
    fn default_terminal_and_excluded_dir_helpers() {
        let mut cfg = AppConfig::default();
        assert_eq!(cfg.default_terminal, "wt");
        assert_eq!(cfg.normalized_terminal(), "wt");

        cfg.set_default_terminal("pwsh");
        assert_eq!(cfg.default_terminal, "pwsh");
        assert_eq!(cfg.normalized_terminal(), "pwsh");

        cfg.set_default_terminal("cmd");
        assert_eq!(cfg.default_terminal, "cmd");
        assert_eq!(cfg.normalized_terminal(), "cmd");

        cfg.set_default_terminal("unknown-shell");
        assert_eq!(cfg.default_terminal, "wt");

        assert!(cfg.add_excluded_index_dir(PathBuf::from(r"C:\Temp")));
        assert!(!cfg.add_excluded_index_dir(PathBuf::from(r"c:\temp\")));
        assert!(!cfg.add_excluded_index_dir(PathBuf::new()));
        assert_eq!(cfg.excluded_index_dirs.len(), 1);

        assert!(cfg.remove_excluded_index_dir(Path::new(r"C:\Temp")));
        assert!(!cfg.remove_excluded_index_dir(Path::new(r"C:\Temp")));
        assert!(cfg.excluded_index_dirs.is_empty());
    }
}
