use onyx_core::{OnyxError, OnyxResult};
use rusqlite::{Connection, OptionalExtension, params};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::warn;

const BUSY_TIMEOUT_SECS: u64 = 5;

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS folder_visits (
    path TEXT PRIMARY KEY,
    visit_count INTEGER NOT NULL,
    last_visited INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_folder_visits_last_visited
    ON folder_visits (last_visited DESC);
CREATE INDEX IF NOT EXISTS idx_folder_visits_count
    ON folder_visits (visit_count DESC, last_visited DESC);

CREATE TABLE IF NOT EXISTS dir_metadata (
    path TEXT PRIMARY KEY,
    entry_count INTEGER NOT NULL,
    total_size INTEGER NOT NULL,
    modified_at INTEGER,
    updated_at INTEGER NOT NULL
);
";

/// Recorded folder visit frequency and timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderVisit {
    pub path: PathBuf,
    pub visit_count: u64,
    /// Unix timestamp in seconds (UTC).
    pub last_visited: i64,
}

/// Cached summary metadata for a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirMetadataRecord {
    pub path: PathBuf,
    pub entry_count: u64,
    pub total_size: u64,
    /// Directory mtime as Unix timestamp in seconds (UTC).
    pub modified_at: Option<i64>,
    /// Cache insertion/update time as Unix timestamp in seconds (UTC).
    pub updated_at: i64,
}

/// SQLite-backed store for folder visit statistics and directory metadata caching.
pub struct MetadataStore {
    conn: Mutex<Connection>,
    db_path: Option<PathBuf>,
}

impl MetadataStore {
    /// Opens (or creates) a SQLite metadata database at `path` using WAL mode,
    /// a busy timeout, and `PRAGMA quick_check`. If the database is corrupt, it is
    /// safely quarantined/removed and recreated cleanly.
    pub fn open(path: &Path) -> OnyxResult<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }

        match open_and_initialize_file(path) {
            Ok(conn) => Ok(Self {
                conn: Mutex::new(conn),
                db_path: Some(path.to_path_buf()),
            }),
            Err(initial_err) => {
                warn!(
                    path = %path.display(),
                    error = %initial_err,
                    "Metadata SQLite DB failed integrity check or open; recovering by recreating"
                );
                quarantine_corrupt_db(path);
                let conn = open_and_initialize_file(path)?;
                Ok(Self {
                    conn: Mutex::new(conn),
                    db_path: Some(path.to_path_buf()),
                })
            }
        }
    }

    /// Opens an in-memory SQLite metadata store (primarily for tests or ephemeral sessions).
    pub fn open_in_memory() -> OnyxResult<Self> {
        let conn = Connection::open_in_memory().map_err(db_err)?;
        configure_connection(&conn, false)?;
        Ok(Self {
            conn: Mutex::new(conn),
            db_path: None,
        })
    }

    /// Returns the filesystem path of the SQLite database, if file-backed.
    #[must_use]
    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }

    /// Records a visit to `path` at the current UTC Unix timestamp.
    pub fn record_visit_now(&self, path: &Path) -> OnyxResult<FolderVisit> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        self.record_visit(path, now)
    }

    /// Records a visit to `path` with an explicit UTC Unix timestamp, incrementing its count.
    pub fn record_visit(&self, path: &Path, timestamp: i64) -> OnyxResult<FolderVisit> {
        let key = path_to_key(path);
        let conn = lock_unpoisoned(&self.conn);
        conn.execute(
            "INSERT INTO folder_visits (path, visit_count, last_visited)
             VALUES (?1, 1, ?2)
             ON CONFLICT(path) DO UPDATE SET
                 visit_count = folder_visits.visit_count + 1,
                 last_visited = MAX(folder_visits.last_visited, excluded.last_visited)",
            params![key, timestamp],
        )
        .map_err(db_err)?;

        let (visit_count, last_visited): (i64, i64) = conn
            .query_row(
                "SELECT visit_count, last_visited FROM folder_visits WHERE path = ?1",
                params![key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(db_err)?;

        Ok(FolderVisit {
            path: path.to_path_buf(),
            visit_count: u64::try_from(visit_count).unwrap_or(0),
            last_visited,
        })
    }

    /// Retrieves visit statistics for a specific folder path.
    pub fn get_visit(&self, path: &Path) -> OnyxResult<Option<FolderVisit>> {
        let key = path_to_key(path);
        let conn = lock_unpoisoned(&self.conn);
        conn.query_row(
            "SELECT path, visit_count, last_visited FROM folder_visits WHERE path = ?1",
            params![key],
            row_to_folder_visit,
        )
        .optional()
        .map_err(db_err)
    }

    /// Returns up to `limit` most frequently visited folders (tie-breaking by recency).
    pub fn top_visited(&self, limit: usize) -> OnyxResult<Vec<FolderVisit>> {
        let conn = lock_unpoisoned(&self.conn);
        let mut stmt = conn
            .prepare(
                "SELECT path, visit_count, last_visited
                 FROM folder_visits
                 ORDER BY visit_count DESC, last_visited DESC
                 LIMIT ?1",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(
                params![i64::try_from(limit).unwrap_or(i64::MAX)],
                row_to_folder_visit,
            )
            .map_err(db_err)?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(db_err)?);
        }
        Ok(results)
    }

    /// Returns up to `limit` most recently visited folders.
    pub fn recent_visits(&self, limit: usize) -> OnyxResult<Vec<FolderVisit>> {
        let conn = lock_unpoisoned(&self.conn);
        let mut stmt = conn
            .prepare(
                "SELECT path, visit_count, last_visited
                 FROM folder_visits
                 ORDER BY last_visited DESC, visit_count DESC
                 LIMIT ?1",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(
                params![i64::try_from(limit).unwrap_or(i64::MAX)],
                row_to_folder_visit,
            )
            .map_err(db_err)?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(db_err)?);
        }
        Ok(results)
    }

    /// Inserts or replaces cached directory metadata.
    pub fn upsert_dir_metadata(&self, record: &DirMetadataRecord) -> OnyxResult<()> {
        let key = path_to_key(&record.path);
        let entry_count = i64::try_from(record.entry_count).unwrap_or(i64::MAX);
        let total_size = i64::try_from(record.total_size).unwrap_or(i64::MAX);
        let conn = lock_unpoisoned(&self.conn);
        conn.execute(
            "INSERT INTO dir_metadata (path, entry_count, total_size, modified_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET
                 entry_count = excluded.entry_count,
                 total_size = excluded.total_size,
                 modified_at = excluded.modified_at,
                 updated_at = excluded.updated_at",
            params![
                key,
                entry_count,
                total_size,
                record.modified_at,
                record.updated_at
            ],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// Retrieves cached metadata for `path`, if present.
    pub fn get_dir_metadata(&self, path: &Path) -> OnyxResult<Option<DirMetadataRecord>> {
        let key = path_to_key(path);
        let conn = lock_unpoisoned(&self.conn);
        conn.query_row(
            "SELECT path, entry_count, total_size, modified_at, updated_at
             FROM dir_metadata
             WHERE path = ?1",
            params![key],
            |row| {
                let path_str: String = row.get(0)?;
                let entry_count: i64 = row.get(1)?;
                let total_size: i64 = row.get(2)?;
                let modified_at: Option<i64> = row.get(3)?;
                let updated_at: i64 = row.get(4)?;
                Ok(DirMetadataRecord {
                    path: PathBuf::from(path_str),
                    entry_count: u64::try_from(entry_count).unwrap_or(0),
                    total_size: u64::try_from(total_size).unwrap_or(0),
                    modified_at,
                    updated_at,
                })
            },
        )
        .optional()
        .map_err(db_err)
    }

    /// Removes cached metadata for `path`. Returns `true` if a row was deleted.
    pub fn delete_dir_metadata(&self, path: &Path) -> OnyxResult<bool> {
        let key = path_to_key(path);
        let conn = lock_unpoisoned(&self.conn);
        let deleted = conn
            .execute("DELETE FROM dir_metadata WHERE path = ?1", params![key])
            .map_err(db_err)?;
        Ok(deleted > 0)
    }

    /// Clears all cached directory metadata rows.
    pub fn clear_dir_metadata(&self) -> OnyxResult<()> {
        let conn = lock_unpoisoned(&self.conn);
        conn.execute("DELETE FROM dir_metadata", [])
            .map_err(db_err)?;
        Ok(())
    }
}

impl fmt::Debug for MetadataStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetadataStore")
            .field("db_path", &self.db_path)
            .finish()
    }
}

fn open_and_initialize_file(path: &Path) -> OnyxResult<Connection> {
    let conn = Connection::open(path).map_err(db_err)?;
    configure_connection(&conn, true)?;
    Ok(conn)
}

fn configure_connection(conn: &Connection, enable_wal: bool) -> OnyxResult<()> {
    conn.busy_timeout(Duration::from_secs(BUSY_TIMEOUT_SECS))
        .map_err(db_err)?;

    let check: String = conn
        .query_row("PRAGMA quick_check;", [], |row| row.get(0))
        .map_err(db_err)?;
    if !check.eq_ignore_ascii_case("ok") {
        return Err(OnyxError::Database {
            message: format!("SQLite quick_check failed: {check}"),
        });
    }

    if enable_wal {
        let _mode: String = conn
            .query_row("PRAGMA journal_mode = WAL;", [], |row| row.get(0))
            .map_err(db_err)?;
        conn.execute_batch("PRAGMA synchronous = NORMAL;")
            .map_err(db_err)?;
    }

    conn.execute_batch(SCHEMA_SQL).map_err(db_err)?;
    Ok(())
}

fn quarantine_corrupt_db(path: &Path) {
    let corrupt_backup = append_suffix(path, ".corrupt");
    let _ = fs::remove_file(&corrupt_backup);
    if fs::rename(path, &corrupt_backup).is_err() {
        let _ = fs::remove_file(path);
    }
    let _ = fs::remove_file(append_suffix(path, "-wal"));
    let _ = fs::remove_file(append_suffix(path, "-shm"));
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

fn path_to_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn row_to_folder_visit(row: &rusqlite::Row<'_>) -> rusqlite::Result<FolderVisit> {
    let path_str: String = row.get(0)?;
    let visit_count: i64 = row.get(1)?;
    let last_visited: i64 = row.get(2)?;
    Ok(FolderVisit {
        path: PathBuf::from(path_str),
        visit_count: u64::try_from(visit_count).unwrap_or(0),
        last_visited,
    })
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn db_err(err: rusqlite::Error) -> OnyxError {
    OnyxError::Database {
        message: err.to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn metadata_store_visits_and_dir_metadata() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("metadata.db");
        let store = MetadataStore::open(&db_path).unwrap();

        let p1 = PathBuf::from(r"C:\Projects");
        let p2 = PathBuf::from(r"D:\Onyx");

        let v1 = store.record_visit(&p1, 100).unwrap();
        assert_eq!(v1.visit_count, 1);
        let v1_again = store.record_visit(&p1, 200).unwrap();
        assert_eq!(v1_again.visit_count, 2);
        assert_eq!(v1_again.last_visited, 200);

        store.record_visit(&p2, 300).unwrap();

        let top = store.top_visited(5).unwrap();
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].path, p1);

        let recent = store.recent_visits(5).unwrap();
        assert_eq!(recent[0].path, p2);
        assert_eq!(store.get_visit(&p1).unwrap().unwrap().visit_count, 2);

        let meta = DirMetadataRecord {
            path: p2.clone(),
            entry_count: 42,
            total_size: 1024 * 1024,
            modified_at: Some(1_700_000_000),
            updated_at: 1_700_000_123,
        };
        store.upsert_dir_metadata(&meta).unwrap();
        assert_eq!(store.get_dir_metadata(&p2).unwrap(), Some(meta));
        assert!(store.delete_dir_metadata(&p2).unwrap());
        assert!(store.get_dir_metadata(&p2).unwrap().is_none());
    }

    #[test]
    fn corrupt_sqlite_db_is_recovered_safely() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("corrupt.db");
        fs::write(
            &db_path,
            b"this is definitely not a valid sqlite database file!",
        )
        .unwrap();

        let store = MetadataStore::open(&db_path).unwrap();
        let visit = store
            .record_visit(Path::new(r"C:\Recovered"), 1_700_000_000)
            .unwrap();
        assert_eq!(visit.visit_count, 1);
        assert_eq!(store.recent_visits(10).unwrap().len(), 1);
    }
}
