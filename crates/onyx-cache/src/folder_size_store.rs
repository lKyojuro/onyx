use crate::memory::CachedFolderSize;
use onyx_core::{OnyxError, OnyxResult, normalize_display_path};
use rusqlite::{Connection, OptionalExtension, params};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::warn;

const BUSY_TIMEOUT_SECS: u64 = 5;

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS folder_sizes (
    path TEXT PRIMARY KEY,
    total_bytes INTEGER NOT NULL,
    file_count INTEGER NOT NULL,
    truncated INTEGER NOT NULL,
    mtime_secs INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
";

/// Returns the directory's modification time as Unix seconds (UTC), if available.
#[must_use]
pub fn dir_mtime_secs(path: &Path) -> Option<i64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let duration = modified.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(duration.as_secs()).ok()
}

fn normalize_folder_key(path: &Path) -> String {
    let normalized = normalize_display_path(path).replace('/', "\\");
    let trimmed = normalized.trim_end_matches('\\');
    if trimmed.len() == 2 && trimmed.as_bytes()[1] == b':' {
        format!("{trimmed}\\").to_ascii_lowercase()
    } else if trimmed.is_empty() {
        normalized.to_ascii_lowercase()
    } else {
        trimmed.to_ascii_lowercase()
    }
}

fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// Persistent SQLite-backed store for recursive folder sizes, validated against directory `mtime_secs`.
pub struct FolderSizeStore {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for FolderSizeStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FolderSizeStore").finish_non_exhaustive()
    }
}

impl FolderSizeStore {
    /// Opens (or creates) a SQLite folder-size cache database at `path` using WAL mode.
    /// If the database is corrupt, it is quarantined/removed and recreated cleanly.
    pub fn open(path: &Path) -> OnyxResult<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }

        match open_and_initialize_file(path) {
            Ok(conn) => Ok(Self {
                conn: Mutex::new(conn),
            }),
            Err(initial_err) => {
                warn!(
                    path = %path.display(),
                    error = %initial_err,
                    "FolderSizeStore SQLite DB failed integrity check or open; recreating"
                );
                quarantine_corrupt_db(path);
                let conn = open_and_initialize_file(path)?;
                Ok(Self {
                    conn: Mutex::new(conn),
                })
            }
        }
    }

    /// Opens an ephemeral in-memory SQLite folder-size store (useful for unit tests).
    pub fn open_in_memory() -> OnyxResult<Self> {
        let conn = Connection::open_in_memory().map_err(map_sqlite_err)?;
        configure_and_migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Looks up a cached folder size for `path`, returning `Some` only if `mtime_secs` matches.
    pub fn get_if_fresh(
        &self,
        path: &Path,
        current_mtime_secs: i64,
    ) -> OnyxResult<Option<CachedFolderSize>> {
        let key = normalize_folder_key(path);
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT total_bytes, file_count, truncated, mtime_secs
                 FROM folder_sizes
                 WHERE path = ?1",
                params![key],
                |row| {
                    let total_bytes: i64 = row.get(0)?;
                    let file_count: i64 = row.get(1)?;
                    let truncated: i64 = row.get(2)?;
                    let mtime_secs: i64 = row.get(3)?;
                    Ok((total_bytes, file_count, truncated, mtime_secs))
                },
            )
            .optional()
            .map_err(map_sqlite_err)?;

        Ok(
            row.and_then(|(total_bytes, file_count, truncated, mtime_secs)| {
                if mtime_secs == current_mtime_secs {
                    Some(CachedFolderSize::new(
                        u64::try_from(total_bytes).unwrap_or(0),
                        u64::try_from(file_count).unwrap_or(0),
                        truncated != 0,
                    ))
                } else {
                    None
                }
            }),
        )
    }

    /// Inserts or updates a cached folder size along with the directory's `mtime_secs`.
    pub fn upsert(&self, path: &Path, size: CachedFolderSize, mtime_secs: i64) -> OnyxResult<()> {
        let key = normalize_folder_key(path);
        let updated_at = unix_now_secs();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO folder_sizes (path, total_bytes, file_count, truncated, mtime_secs, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(path) DO UPDATE SET
                total_bytes = excluded.total_bytes,
                file_count = excluded.file_count,
                truncated = excluded.truncated,
                mtime_secs = excluded.mtime_secs,
                updated_at = excluded.updated_at",
            params![
                key,
                i64::try_from(size.total_bytes).unwrap_or(i64::MAX),
                i64::try_from(size.file_count).unwrap_or(i64::MAX),
                i64::from(size.truncated),
                mtime_secs,
                updated_at,
            ],
        )
        .map_err(map_sqlite_err)?;
        Ok(())
    }

    /// Deletes cached entries for `path` and all of its ancestor directories.
    /// Returns the total number of rows removed.
    pub fn invalidate_with_ancestors(&self, path: &Path) -> OnyxResult<usize> {
        let mut keys = Vec::new();
        for ancestor in path.ancestors() {
            if ancestor.as_os_str().is_empty() {
                continue;
            }
            let key = normalize_folder_key(ancestor);
            if !key.is_empty() && !keys.contains(&key) {
                keys.push(key);
            }
        }
        if keys.is_empty() {
            return Ok(0);
        }

        let mut conn = self.lock();
        let tx = conn.transaction().map_err(map_sqlite_err)?;
        let mut removed = 0usize;
        {
            let mut stmt = tx
                .prepare("DELETE FROM folder_sizes WHERE path = ?1")
                .map_err(map_sqlite_err)?;
            for key in keys {
                removed += stmt.execute(params![key]).map_err(map_sqlite_err)?;
            }
        }
        tx.commit().map_err(map_sqlite_err)?;
        Ok(removed)
    }

    /// Removes all cached folder size entries.
    pub fn clear(&self) -> OnyxResult<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM folder_sizes", [])
            .map_err(map_sqlite_err)?;
        Ok(())
    }
}

fn open_and_initialize_file(path: &Path) -> OnyxResult<Connection> {
    let conn = Connection::open(path).map_err(map_sqlite_err)?;
    configure_and_migrate(&conn)?;
    verify_quick_check(&conn)?;
    Ok(conn)
}

fn configure_and_migrate(conn: &Connection) -> OnyxResult<()> {
    conn.busy_timeout(Duration::from_secs(BUSY_TIMEOUT_SECS))
        .map_err(map_sqlite_err)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;",
    )
    .map_err(map_sqlite_err)?;
    conn.execute_batch(SCHEMA_SQL).map_err(map_sqlite_err)?;
    Ok(())
}

fn verify_quick_check(conn: &Connection) -> OnyxResult<()> {
    let status: String = conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(map_sqlite_err)?;
    if status.eq_ignore_ascii_case("ok") {
        Ok(())
    } else {
        Err(OnyxError::Database {
            message: format!("SQLite quick_check failed: {status}"),
        })
    }
}

fn quarantine_corrupt_db(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut os = path.as_os_str().to_os_string();
        os.push(suffix);
        let candidate = PathBuf::from(os);
        if candidate.exists() {
            let mut corrupt_os = candidate.as_os_str().to_os_string();
            corrupt_os.push(".corrupt");
            let corrupt_path = PathBuf::from(corrupt_os);
            if fs::rename(&candidate, &corrupt_path).is_err() {
                let _ = fs::remove_file(&candidate);
            }
        }
    }
}

fn map_sqlite_err(err: rusqlite::Error) -> OnyxError {
    OnyxError::Database {
        message: format!("folder size store sqlite error: {err}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn folder_size_store_save_load_and_mtime_invalidation() {
        let store = FolderSizeStore::open_in_memory().unwrap();
        let path = Path::new(r"D:\Workspace\Project");
        let size = CachedFolderSize::new(42_000, 17, false);

        store.upsert(path, size, 1_700_000_100).unwrap();

        // Matching mtime returns cached size (case & trailing-slash insensitive).
        assert_eq!(
            store
                .get_if_fresh(Path::new(r"d:\workspace\project\"), 1_700_000_100)
                .unwrap(),
            Some(size)
        );

        // Changed mtime invalidates the lookup.
        assert_eq!(store.get_if_fresh(path, 1_700_000_200).unwrap(), None);

        // Upserting with new mtime replaces the old value.
        let updated = CachedFolderSize::new(99_000, 30, true);
        store.upsert(path, updated, 1_700_000_200).unwrap();
        assert_eq!(
            store.get_if_fresh(path, 1_700_000_200).unwrap(),
            Some(updated)
        );
    }

    #[test]
    fn folder_size_store_invalidates_path_and_ancestors() {
        let store = FolderSizeStore::open_in_memory().unwrap();
        let root = Path::new(r"D:\Work");
        let child = Path::new(r"D:\Work\Alpha");
        let grandchild = Path::new(r"D:\Work\Alpha\Nested");
        let sibling = Path::new(r"D:\Work\Beta");

        store
            .upsert(root, CachedFolderSize::new(1000, 10, false), 100)
            .unwrap();
        store
            .upsert(child, CachedFolderSize::new(600, 6, false), 100)
            .unwrap();
        store
            .upsert(grandchild, CachedFolderSize::new(200, 2, false), 100)
            .unwrap();
        store
            .upsert(sibling, CachedFolderSize::new(400, 4, false), 100)
            .unwrap();

        let removed = store.invalidate_with_ancestors(grandchild).unwrap();
        assert_eq!(removed, 3);
        assert_eq!(store.get_if_fresh(grandchild, 100).unwrap(), None);
        assert_eq!(store.get_if_fresh(child, 100).unwrap(), None);
        assert_eq!(store.get_if_fresh(root, 100).unwrap(), None);
        assert_eq!(
            store.get_if_fresh(sibling, 100).unwrap(),
            Some(CachedFolderSize::new(400, 4, false))
        );
    }

    #[test]
    fn folder_size_store_corrupt_db_recovers_cleanly() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("folder_sizes.db");
        fs::write(&db_path, b"not a valid sqlite database").unwrap();

        let store = FolderSizeStore::open(&db_path).unwrap();
        let path = Path::new(r"C:\Temp");
        store
            .upsert(path, CachedFolderSize::new(123, 1, false), 555)
            .unwrap();
        assert_eq!(
            store.get_if_fresh(path, 555).unwrap(),
            Some(CachedFolderSize::new(123, 1, false))
        );
    }
}
