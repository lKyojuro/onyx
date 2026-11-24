use crate::model::{AppConfig, SessionState};
use onyx_core::{OnyxError, OnyxResult};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::warn;

const CONFIG_FILE_NAME: &str = "config.toml";
const SESSION_FILE_NAME: &str = "session.json";

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
            Ok(session) => session,
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
    let tmp_path = append_suffix(target, &format!(".tmp.{pid}"));

    let write_res = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;
        file.write_all(data)?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(OnyxError::Io(err));
    }

    if let Err(primary_err) = fs::rename(&tmp_path, target) {
        if target.exists() {
            // Safe Windows fallback: move existing target to a backup file before renaming temp into place.
            let backup_path = append_suffix(target, &format!(".bak.{pid}"));
            let _ = fs::remove_file(&backup_path);
            if fs::rename(target, &backup_path).is_ok() {
                match fs::rename(&tmp_path, target) {
                    Ok(()) => {
                        let _ = fs::remove_file(&backup_path);
                        return Ok(());
                    }
                    Err(replace_err) => {
                        let _ = fs::rename(&backup_path, target);
                        let _ = fs::remove_file(&tmp_path);
                        return Err(OnyxError::Io(replace_err));
                    }
                }
            }
        }
        let _ = fs::remove_file(&tmp_path);
        return Err(OnyxError::Io(primary_err));
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
            favorites: vec![FavoriteItem {
                name: "Projects".to_string(),
                path: PathBuf::from(r"D:\Projects"),
            }],
            excluded_index_dirs: vec![PathBuf::from(r"C:\Windows")],
            recent_locations: vec![PathBuf::from(r"D:\Onyx")],
        };

        store.save_config(&cfg).unwrap();
        assert_eq!(store.load_config(), cfg);

        // Overwrite existing file cleanly without leaving temp files behind.
        cfg.theme = ThemeMode::System;
        cfg.accent_hex = Some("#FF5500".to_string());
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
        };

        store.save_session(&session).unwrap();
        assert_eq!(store.load_session(), session);
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
}
