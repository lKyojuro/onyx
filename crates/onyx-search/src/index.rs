use crate::types::{
    CompiledQuery, IndexStats, SearchQuery, SearchResult, SearchSource, canonical_db_path,
    current_unix_secs, file_entry_from_metadata, is_safe_traversable_dir, normalize_extension,
    path_starts_with_dir, score_filename, sort_search_results,
};
use onyx_core::{
    FileAttributes, FileEntry, FileKind, OnyxError, OnyxResult, infer_type_label,
    to_extended_length_path,
};
use rusqlite::{Connection, OptionalExtension, params, types::Value};
use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

const INDEX_BATCH_SIZE: usize = 512;
const CANCEL_CHECK_INTERVAL: usize = 64;

/// Phase B SQLite WAL + FTS5 persistent search index.
pub struct SearchIndex {
    conn: Mutex<Connection>,
    excluded_dirs: Vec<PathBuf>,
}

impl fmt::Debug for SearchIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SearchIndex")
            .field("excluded_dirs", &self.excluded_dirs)
            .finish_non_exhaustive()
    }
}

impl SearchIndex {
    /// Opens or creates an on-disk SQLite search index at `db_path` using WAL mode.
    pub fn open(db_path: &Path) -> OnyxResult<Self> {
        if let Some(parent) = db_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(db_path).map_err(db_err)?;
        configure_connection(&conn, true)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            excluded_dirs: Vec::new(),
        })
    }

    /// Opens an isolated in-memory SQLite search index (ideal for tests and ephemeral caches).
    pub fn open_in_memory() -> OnyxResult<Self> {
        let conn = Connection::open_in_memory().map_err(db_err)?;
        configure_connection(&conn, false)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            excluded_dirs: Vec::new(),
        })
    }

    /// Returns the currently configured excluded directories.
    #[must_use]
    pub fn excluded_dirs(&self) -> &[PathBuf] {
        &self.excluded_dirs
    }

    /// Replaces the list of excluded directories.
    pub fn set_excluded_dirs(&mut self, dirs: Vec<PathBuf>) {
        self.excluded_dirs = dirs;
    }

    /// Appends a single excluded directory path or folder name.
    pub fn add_excluded_dir(&mut self, dir: PathBuf) {
        self.excluded_dirs.push(dir);
    }

    /// Builder-style configuration for excluded directories.
    #[must_use]
    pub fn with_excluded_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.excluded_dirs = dirs;
        self
    }

    /// Recursively indexes all files and directories under `root` using batched transactions.
    ///
    /// Skips configured excluded directories as well as symlink and reparse-point loops.
    pub fn index_directory(&self, root: &Path, cancel: &CancellationToken) -> OnyxResult<usize> {
        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        let root_ext = to_extended_length_path(root);
        let root_meta = match std::fs::symlink_metadata(&root_ext) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(OnyxError::NotFound {
                    path: root.to_path_buf(),
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(OnyxError::PermissionDenied {
                    path: root.to_path_buf(),
                });
            }
            Err(err) => return Err(OnyxError::Io(err)),
        };

        if !root_meta.is_dir() {
            return Err(OnyxError::InvalidPath {
                message: format!("index root is not a directory: {}", root.display()),
            });
        }

        let mut queue = VecDeque::new();
        queue.push_back(root.to_path_buf());

        let mut pending = Vec::with_capacity(INDEX_BATCH_SIZE);
        let mut total_indexed = 0usize;
        let mut scanned = 0usize;

        while let Some(dir) = queue.pop_front() {
            if cancel.is_cancelled() {
                return Err(OnyxError::Cancelled);
            }
            if self.is_excluded(&dir) {
                continue;
            }

            let dir_ext = to_extended_length_path(&dir);
            let read_dir = match std::fs::read_dir(&dir_ext) {
                Ok(rd) => rd,
                Err(err) => {
                    tracing::debug!(path = %dir.display(), error = %err, "skipping unreadable dir during indexing");
                    continue;
                }
            };

            for entry_res in read_dir {
                scanned += 1;
                if scanned.is_multiple_of(CANCEL_CHECK_INTERVAL) && cancel.is_cancelled() {
                    return Err(OnyxError::Cancelled);
                }

                let Ok(dir_entry) = entry_res else {
                    continue;
                };

                let entry_path = dir.join(dir_entry.file_name());
                if self.is_excluded(&entry_path) {
                    continue;
                }

                let entry_ext = to_extended_length_path(&entry_path);
                let Ok(meta) = std::fs::symlink_metadata(&entry_ext) else {
                    continue;
                };

                let file_entry = file_entry_from_metadata(&entry_path, &meta);
                if is_safe_traversable_dir(&meta) {
                    queue.push_back(entry_path);
                }

                pending.push(file_entry);
                if pending.len() >= INDEX_BATCH_SIZE {
                    let count = self.upsert_batch(&pending)?;
                    total_indexed += count;
                    pending.clear();
                }
            }
        }

        if !pending.is_empty() {
            let count = self.upsert_batch(&pending)?;
            total_indexed += count;
        }

        self.set_last_indexed_now()?;
        Ok(total_indexed)
    }

    /// Inserts or updates a single [`FileEntry`] in the index and FTS5 table.
    pub fn upsert_entry(&self, entry: &FileEntry) -> OnyxResult<()> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction().map_err(db_err)?;
        upsert_entry_in_tx(&tx, entry)?;
        set_meta_in_tx(&tx, "last_indexed_at", &current_unix_secs().to_string())?;
        tx.commit().map_err(db_err)?;
        Ok(())
    }

    /// Removes `path` and any indexed descendants under `path` from the index.
    pub fn remove_path(&self, path: &Path) -> OnyxResult<()> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction().map_err(db_err)?;

        let exact = canonical_db_path(path);
        let like_pattern = dir_descendants_like_pattern(&exact);

        tx.execute(
            "DELETE FROM indexed_files WHERE LOWER(path) = LOWER(?1) OR LOWER(path) LIKE LOWER(?2) ESCAPE '!'",
            params![exact, like_pattern],
        )
        .map_err(db_err)?;

        tx.commit().map_err(db_err)?;
        Ok(())
    }

    /// Removes stale indexed entries under `root` whose files no longer exist on disk.
    ///
    /// Returns the number of pruned entries.
    pub fn reconcile_directory(&self, root: &Path) -> OnyxResult<usize> {
        let root_str = canonical_db_path(root);
        let like_pattern = dir_descendants_like_pattern(&root_str);

        let paths: Vec<String> = {
            let conn = self.lock_conn()?;
            let mut stmt = conn
                .prepare(
                    "SELECT path FROM indexed_files WHERE LOWER(path) = LOWER(?1) OR LOWER(path) LIKE LOWER(?2) ESCAPE '!'",
                )
                .map_err(db_err)?;
            let rows = stmt
                .query_map(params![root_str, like_pattern], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(db_err)?;

            let mut collected = Vec::new();
            for row in rows {
                collected.push(row.map_err(db_err)?);
            }
            collected
        };

        let mut stale = Vec::new();
        for p in paths {
            let ext_path = to_extended_length_path(Path::new(&p));
            if std::fs::symlink_metadata(&ext_path).is_err() {
                stale.push(p);
            }
        }

        if stale.is_empty() {
            return Ok(0);
        }

        let mut conn = self.lock_conn()?;
        let tx = conn.transaction().map_err(db_err)?;
        {
            let mut del_stmt = tx
                .prepare("DELETE FROM indexed_files WHERE path = ?1")
                .map_err(db_err)?;
            for p in &stale {
                del_stmt.execute(params![p]).map_err(db_err)?;
            }
        }
        tx.commit().map_err(db_err)?;
        Ok(stale.len())
    }

    /// Queries the SQLite FTS5 index with safe query escaping and SQL metadata filters.
    pub fn query(&self, root: Option<&Path>, query: &SearchQuery) -> OnyxResult<Vec<SearchResult>> {
        if query.max_results == 0 {
            return Ok(Vec::new());
        }

        let compiled = CompiledQuery::from_query(query);
        let fts_expr = build_safe_fts5_query(&query.text);

        let mut sql = String::new();
        let mut values: Vec<Value> = Vec::new();

        if let Some(ref expr) = fts_expr {
            sql.push_str(
                "SELECT f.path, f.name, f.extension, f.size, f.modified, f.is_dir \
                 FROM indexed_files f \
                 JOIN files_fts ON files_fts.rowid = f.rowid \
                 WHERE files_fts MATCH ?",
            );
            values.push(Value::Text(expr.clone()));
        } else {
            sql.push_str(
                "SELECT f.path, f.name, f.extension, f.size, f.modified, f.is_dir \
                 FROM indexed_files f \
                 WHERE 1=1",
            );
            // If user entered punctuation-only tokens (no FTS5 word tokens), narrow in SQL with LIKE.
            for tok in &compiled.tokens {
                sql.push_str(" AND LOWER(f.name) LIKE LOWER(?) ESCAPE '!'");
                values.push(Value::Text(format!("%{}%", escape_like(tok))));
            }
        }

        if let Some(root_path) = root {
            let root_str = canonical_db_path(root_path);
            let like_pattern = dir_descendants_like_pattern(&root_str);
            sql.push_str(" AND LOWER(f.path) LIKE LOWER(?) ESCAPE '!'");
            values.push(Value::Text(like_pattern));
        }

        if !compiled.extensions.is_empty() {
            sql.push_str(" AND LOWER(COALESCE(f.extension, '')) IN (");
            for (idx, ext) in compiled.extensions.iter().enumerate() {
                if idx > 0 {
                    sql.push_str(", ");
                }
                sql.push('?');
                values.push(Value::Text(ext.clone()));
            }
            sql.push(')');
        }

        if let Some(min_size) = compiled.min_size {
            let min_i64 = i64::try_from(min_size).unwrap_or(i64::MAX);
            sql.push_str(" AND f.size >= ?");
            values.push(Value::Integer(min_i64));
        }

        if let Some(max_size) = compiled.max_size {
            let max_i64 = i64::try_from(max_size).unwrap_or(i64::MAX);
            sql.push_str(" AND f.size <= ?");
            values.push(Value::Integer(max_i64));
        }

        if let Some(after) = compiled.modified_after {
            sql.push_str(" AND f.modified IS NOT NULL AND f.modified >= ?");
            values.push(Value::Integer(after));
        }

        if let Some(before) = compiled.modified_before {
            sql.push_str(" AND f.modified IS NOT NULL AND f.modified <= ?");
            values.push(Value::Integer(before));
        }

        // Over-fetch slightly so multi-token substring verification and re-ranking produce optimal top-N.
        let sql_limit = compiled.max_results.saturating_mul(4).clamp(200, 10_000);
        sql.push_str(" LIMIT ?");
        values.push(Value::Integer(i64::try_from(sql_limit).unwrap_or(10_000)));

        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(&sql).map_err(db_err)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                let path_str: String = row.get(0)?;
                let name: String = row.get(1)?;
                let extension: Option<String> = row.get(2)?;
                let size_i64: i64 = row.get(3)?;
                let modified: Option<i64> = row.get(4)?;
                let is_dir_int: i64 = row.get(5)?;
                Ok((
                    path_str,
                    name,
                    extension,
                    size_i64,
                    modified,
                    is_dir_int != 0,
                ))
            })
            .map_err(db_err)?;

        let mut results = Vec::new();
        for row_res in rows {
            let (path_str, name, extension, size_i64, modified, is_dir) =
                row_res.map_err(db_err)?;

            let hidden = name.starts_with('.');
            if !compiled.include_hidden && hidden {
                continue;
            }

            let Some(score) = score_filename(&name, &compiled.full_lower, &compiled.tokens) else {
                continue;
            };

            let kind = if is_dir {
                FileKind::Directory
            } else {
                FileKind::File
            };
            let size = u64::try_from(size_i64).unwrap_or(0);
            let type_label = infer_type_label(kind, extension.as_deref());

            let entry = FileEntry {
                path: PathBuf::from(path_str),
                name,
                extension,
                kind,
                size,
                modified,
                created: None,
                attributes: FileAttributes {
                    hidden,
                    ..FileAttributes::default()
                },
                is_symlink: false,
                symlink_target: None,
                type_label,
            };

            results.push(SearchResult {
                entry,
                score,
                source: SearchSource::SqliteIndex,
            });
        }

        sort_search_results(&mut results);
        results.truncate(compiled.max_results);
        Ok(results)
    }

    /// Returns whether the index contains at least one entry under `root`.
    pub fn has_entries_under(&self, root: &Path) -> OnyxResult<bool> {
        let root_str = canonical_db_path(root);
        let like_pattern = dir_descendants_like_pattern(&root_str);

        let conn = self.lock_conn()?;
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM indexed_files WHERE LOWER(path) LIKE LOWER(?1) ESCAPE '!' LIMIT 1",
                params![like_pattern],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)?;
        Ok(found.is_some())
    }

    /// Returns summary statistics about the search index.
    pub fn stats(&self) -> OnyxResult<IndexStats> {
        let conn = self.lock_conn()?;
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM indexed_files", [], |row| row.get(0))
            .map_err(db_err)?;
        let last_indexed_str: Option<String> = conn
            .query_row(
                "SELECT value FROM index_meta WHERE key = 'last_indexed_at'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)?;

        let last_indexed_at = last_indexed_str.and_then(|s| s.parse::<i64>().ok());

        Ok(IndexStats {
            total_entries: usize::try_from(total).unwrap_or(0),
            last_indexed_at,
        })
    }

    fn is_excluded(&self, path: &Path) -> bool {
        if self.excluded_dirs.is_empty() {
            return false;
        }
        let candidate = canonical_db_path(path);
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        for excluded in &self.excluded_dirs {
            if excluded.is_absolute() {
                let excluded_str = canonical_db_path(excluded);
                if path_starts_with_dir(&candidate, &excluded_str) {
                    return true;
                }
            } else {
                let excluded_name = excluded.to_string_lossy();
                #[cfg(windows)]
                let matches = file_name.eq_ignore_ascii_case(&excluded_name);
                #[cfg(not(windows))]
                let matches = file_name == excluded_name.as_ref();
                if matches {
                    return true;
                }
            }
        }
        false
    }

    fn upsert_batch(&self, entries: &[FileEntry]) -> OnyxResult<usize> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction().map_err(db_err)?;
        for entry in entries {
            upsert_entry_in_tx(&tx, entry)?;
        }
        tx.commit().map_err(db_err)?;
        Ok(entries.len())
    }

    fn set_last_indexed_now(&self) -> OnyxResult<()> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction().map_err(db_err)?;
        set_meta_in_tx(&tx, "last_indexed_at", &current_unix_secs().to_string())?;
        tx.commit().map_err(db_err)?;
        Ok(())
    }

    fn lock_conn(&self) -> OnyxResult<std::sync::MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| OnyxError::Database {
            message: "search index database lock poisoned".to_string(),
        })
    }
}

fn configure_connection(conn: &Connection, enable_wal: bool) -> OnyxResult<()> {
    if enable_wal {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(db_err)?;
    }
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(db_err)?;
    conn.pragma_update(None, "temp_store", "MEMORY")
        .map_err(db_err)?;
    conn.pragma_update(None, "busy_timeout", 5000)
        .map_err(db_err)?;
    Ok(())
}

fn init_schema(conn: &Connection) -> OnyxResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS indexed_files (
            path TEXT PRIMARY KEY,
            parent_dir TEXT NOT NULL,
            name TEXT NOT NULL,
            extension TEXT,
            size INTEGER NOT NULL,
            modified INTEGER,
            is_dir INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_indexed_files_parent ON indexed_files(parent_dir);
        CREATE INDEX IF NOT EXISTS idx_indexed_files_ext ON indexed_files(extension);

        CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(
            name,
            path,
            content='indexed_files',
            content_rowid='rowid',
            tokenize='unicode61'
        );

        CREATE TRIGGER IF NOT EXISTS indexed_files_ai AFTER INSERT ON indexed_files BEGIN
            INSERT INTO files_fts(rowid, name, path) VALUES (new.rowid, new.name, new.path);
        END;

        CREATE TRIGGER IF NOT EXISTS indexed_files_ad AFTER DELETE ON indexed_files BEGIN
            INSERT INTO files_fts(files_fts, rowid, name, path) VALUES ('delete', old.rowid, old.name, old.path);
        END;

        CREATE TRIGGER IF NOT EXISTS indexed_files_au AFTER UPDATE ON indexed_files BEGIN
            INSERT INTO files_fts(files_fts, rowid, name, path) VALUES ('delete', old.rowid, old.name, old.path);
            INSERT INTO files_fts(rowid, name, path) VALUES (new.rowid, new.name, new.path);
        END;

        CREATE TABLE IF NOT EXISTS index_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );",
    )
    .map_err(db_err)
}

fn upsert_entry_in_tx(tx: &rusqlite::Transaction<'_>, entry: &FileEntry) -> OnyxResult<()> {
    let path_str = canonical_db_path(&entry.path);
    let parent_dir = entry
        .path
        .parent()
        .map(canonical_db_path)
        .unwrap_or_default();
    let ext = entry
        .extension
        .as_deref()
        .map(normalize_extension)
        .filter(|e| !e.is_empty());
    let size = i64::try_from(entry.size).unwrap_or(i64::MAX);
    let is_dir = i64::from(entry.is_dir());

    tx.execute(
        "INSERT INTO indexed_files (path, parent_dir, name, extension, size, modified, is_dir)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(path) DO UPDATE SET
             parent_dir = excluded.parent_dir,
             name = excluded.name,
             extension = excluded.extension,
             size = excluded.size,
             modified = excluded.modified,
             is_dir = excluded.is_dir",
        params![
            path_str,
            parent_dir,
            entry.name,
            ext,
            size,
            entry.modified,
            is_dir
        ],
    )
    .map_err(db_err)?;

    Ok(())
}

fn set_meta_in_tx(tx: &rusqlite::Transaction<'_>, key: &str, value: &str) -> OnyxResult<()> {
    tx.execute(
        "INSERT INTO index_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map_err(db_err)?;
    Ok(())
}

/// Converts arbitrary user input into a safe FTS5 `MATCH` expression against the `name` column.
///
/// Extracts alphanumeric runs (matching SQLite's `unicode61` tokenizer), wraps each in double
/// quotes (preventing any FTS5 operator or special-character syntax errors), and appends `*`
/// for prefix matching. Returns `None` if no searchable alphanumeric tokens exist.
pub(crate) fn build_safe_fts5_query(raw: &str) -> Option<String> {
    let mut terms = Vec::new();
    let mut current = String::new();

    for ch in raw.chars() {
        if ch.is_alphanumeric() {
            current.push(ch);
        } else if !current.is_empty() {
            terms.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        terms.push(current);
    }

    if terms.is_empty() {
        return None;
    }

    let clauses: Vec<String> = terms
        .into_iter()
        .map(|term| {
            let escaped = term.replace('"', "\"\"");
            format!("name : \"{escaped}\"*")
        })
        .collect();

    Some(clauses.join(" AND "))
}

/// Escapes SQL `LIKE` wildcards using `'!'` as the escape character (safe with Windows `\`).
fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if matches!(ch, '!' | '%' | '_') {
            out.push('!');
        }
        out.push(ch);
    }
    out
}

fn dir_descendants_like_pattern(root_str: &str) -> String {
    let sep = if cfg!(windows) { '\\' } else { '/' };
    let trimmed = root_str.trim_end_matches(['\\', '/']);
    let mut prefix = escape_like(trimmed);
    prefix.push(sep);
    prefix.push('%');
    prefix
}

fn db_err(err: rusqlite::Error) -> OnyxError {
    OnyxError::Database {
        message: err.to_string(),
    }
}
