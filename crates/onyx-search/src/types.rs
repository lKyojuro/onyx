use onyx_core::{FileAttributes, FileEntry, FileKind, infer_type_label, normalize_display_path};
use std::cmp::Ordering;
use std::fs::Metadata;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Default maximum number of results returned by a search query.
pub const DEFAULT_MAX_RESULTS: usize = 500;

/// Parameters controlling live and indexed file search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    pub text: String,
    pub extensions: Vec<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub modified_after: Option<i64>,
    pub modified_before: Option<i64>,
    pub include_hidden: bool,
    pub max_results: usize,
    pub use_index: bool,
}

impl Default for SearchQuery {
    fn default() -> Self {
        Self {
            text: String::new(),
            extensions: Vec::new(),
            min_size: None,
            max_size: None,
            modified_after: None,
            modified_before: None,
            include_hidden: false,
            max_results: DEFAULT_MAX_RESULTS,
            use_index: true,
        }
    }
}

/// Origin backend that produced a [`SearchResult`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchSource {
    LiveWalk,
    SqliteIndex,
}

/// A single search hit with relevance score and source metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    pub entry: FileEntry,
    pub score: f32,
    pub source: SearchSource,
}

/// Summary statistics for the SQLite FTS5 search index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IndexStats {
    pub total_entries: usize,
    pub last_indexed_at: Option<i64>,
}

/// Pre-normalized query representation for fast repeated matching.
#[derive(Debug, Clone)]
pub(crate) struct CompiledQuery {
    pub full_lower: String,
    pub tokens: Vec<String>,
    pub extensions: Vec<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub modified_after: Option<i64>,
    pub modified_before: Option<i64>,
    pub include_hidden: bool,
    pub max_results: usize,
}

impl CompiledQuery {
    pub(crate) fn from_query(query: &SearchQuery) -> Self {
        let trimmed = query.text.trim();
        let cleaned_tokens: Vec<String> = trimmed
            .split_whitespace()
            .flat_map(|raw_tok| {
                let unquoted = raw_tok.trim_matches('"');
                unquoted
                    .split('*')
                    .map(|part| part.trim_matches('"').to_lowercase())
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
            })
            .collect();

        let (full_lower, tokens) = if !trimmed.is_empty() && cleaned_tokens.is_empty() {
            let raw_lower = trimmed.to_lowercase();
            let raw_tokens = trimmed
                .split_whitespace()
                .map(str::to_lowercase)
                .filter(|t| !t.is_empty())
                .collect();
            (raw_lower, raw_tokens)
        } else {
            let joined = cleaned_tokens.join(" ");
            (joined, cleaned_tokens)
        };

        let extensions = query
            .extensions
            .iter()
            .map(|ext| normalize_extension(ext))
            .filter(|ext| !ext.is_empty())
            .collect();

        Self {
            full_lower,
            tokens,
            extensions,
            min_size: query.min_size,
            max_size: query.max_size,
            modified_after: query.modified_after,
            modified_before: query.modified_before,
            include_hidden: query.include_hidden,
            max_results: query.max_results,
        }
    }

    /// Evaluates whether an entry passes all filters and returns its relevance score if matched.
    pub(crate) fn match_entry(&self, entry: &FileEntry) -> Option<f32> {
        if !self.include_hidden && entry.attributes.hidden {
            return None;
        }
        if !self.extensions.is_empty() {
            let entry_ext = entry
                .extension
                .as_deref()
                .map(normalize_extension)
                .unwrap_or_default();
            if entry_ext.is_empty() || !self.extensions.iter().any(|e| e == &entry_ext) {
                return None;
            }
        }
        if let Some(min_size) = self.min_size
            && entry.size < min_size
        {
            return None;
        }
        if let Some(max_size) = self.max_size
            && entry.size > max_size
        {
            return None;
        }
        if let Some(after) = self.modified_after {
            match entry.modified {
                Some(m) if m >= after => {}
                _ => return None,
            }
        }
        if let Some(before) = self.modified_before {
            match entry.modified {
                Some(m) if m <= before => {}
                _ => return None,
            }
        }

        score_filename(&entry.name, &self.full_lower, &self.tokens)
    }
}

/// Normalizes a file extension (strips leading dots and lowercases).
pub(crate) fn normalize_extension(ext: &str) -> String {
    ext.trim().trim_start_matches('.').to_ascii_lowercase()
}

/// Scores a filename against a case-insensitive multi-token query.
/// Returns `None` if any query token is missing from the filename.
pub(crate) fn score_filename(name: &str, full_lower: &str, tokens: &[String]) -> Option<f32> {
    if tokens.is_empty() {
        return Some(1.0);
    }

    let name_lower = name.to_lowercase();

    for token in tokens {
        if !name_lower.contains(token.as_str()) {
            return None;
        }
    }

    if name_lower == full_lower {
        return Some(100.0);
    }

    let stem_lower = Path::new(&name_lower)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&name_lower);
    if stem_lower == full_lower {
        return Some(90.0);
    }

    if name_lower.starts_with(full_lower) {
        let ratio = (full_lower.len() as f32 / name_lower.len().max(1) as f32).clamp(0.0, 1.0);
        return Some(80.0 + ratio * 9.0);
    }

    let mut score = 40.0_f32;
    if let Some(first) = tokens.first()
        && name_lower.starts_with(first.as_str())
    {
        score += 15.0;
    }

    let mut matched_chars = 0usize;
    for token in tokens {
        matched_chars += token.len();
        if let Some(pos) = name_lower.find(token.as_str()) {
            if is_word_boundary(&name_lower, pos) {
                score += 10.0;
            } else {
                score += 5.0;
            }
        }
    }

    let density = (matched_chars as f32 / name_lower.len().max(1) as f32).clamp(0.0, 1.0);
    score += density * 15.0;

    Some(score.min(79.5))
}

fn is_word_boundary(haystack: &str, byte_pos: usize) -> bool {
    if byte_pos == 0 {
        return true;
    }
    haystack[..byte_pos]
        .chars()
        .next_back()
        .is_some_and(|ch| matches!(ch, '_' | '-' | '.' | ' ' | '(' | '[' | '/'))
}

/// Deterministic ordering: highest score first, then case-insensitive name, then path.
pub(crate) fn sort_search_results(results: &mut [SearchResult]) {
    results.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| {
                a.entry
                    .name
                    .to_lowercase()
                    .cmp(&b.entry.name.to_lowercase())
            })
            .then_with(|| a.entry.path.cmp(&b.entry.path))
    });
}

/// Converts a `SystemTime` into UTC Unix seconds.
pub(crate) fn system_time_to_unix_secs(time: SystemTime) -> Option<i64> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(dur) => i64::try_from(dur.as_secs()).ok(),
        Err(err) => i64::try_from(err.duration().as_secs())
            .ok()
            .and_then(i64::checked_neg),
    }
}

/// Current UTC Unix timestamp in seconds.
pub(crate) fn current_unix_secs() -> i64 {
    system_time_to_unix_secs(SystemTime::now()).unwrap_or(0)
}

/// Builds a [`FileEntry`] from a filesystem path and its non-following `symlink_metadata`.
pub(crate) fn file_entry_from_metadata(path: &Path, meta: &Metadata) -> FileEntry {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| normalize_display_path(path));

    let attributes = extract_attributes(&name, meta);
    let is_symlink = meta.file_type().is_symlink();

    let kind = if is_symlink {
        FileKind::Symlink
    } else if attributes.reparse_point {
        FileKind::ReparsePoint
    } else if meta.is_dir() {
        FileKind::Directory
    } else if meta.is_file() {
        FileKind::File
    } else {
        FileKind::Other
    };

    let extension = if matches!(kind, FileKind::Directory) {
        None
    } else {
        path.extension().map(|e| e.to_string_lossy().into_owned())
    };

    let size = if meta.is_dir() { 0 } else { meta.len() };
    let modified = meta.modified().ok().and_then(system_time_to_unix_secs);
    let created = meta.created().ok().and_then(system_time_to_unix_secs);
    let symlink_target = if is_symlink {
        std::fs::read_link(path).ok()
    } else {
        None
    };
    let type_label = infer_type_label(kind, extension.as_deref());

    FileEntry {
        path: Path::new(&normalize_display_path(path)).to_path_buf(),
        name,
        extension,
        kind,
        size,
        modified,
        created,
        attributes,
        is_symlink,
        symlink_target,
        type_label,
    }
}

#[cfg(windows)]
fn extract_attributes(name: &str, meta: &Metadata) -> FileAttributes {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x0000_0004;
    const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x0000_0800;
    const FILE_ATTRIBUTE_ENCRYPTED: u32 = 0x0000_4000;

    let raw = meta.file_attributes();
    FileAttributes {
        hidden: (raw & FILE_ATTRIBUTE_HIDDEN) != 0 || name.starts_with('.'),
        system: (raw & FILE_ATTRIBUTE_SYSTEM) != 0,
        readonly: (raw & FILE_ATTRIBUTE_READONLY) != 0,
        archive: (raw & FILE_ATTRIBUTE_ARCHIVE) != 0,
        compressed: (raw & FILE_ATTRIBUTE_COMPRESSED) != 0,
        encrypted: (raw & FILE_ATTRIBUTE_ENCRYPTED) != 0,
        reparse_point: (raw & FILE_ATTRIBUTE_REPARSE_POINT) != 0,
    }
}

#[cfg(not(windows))]
fn extract_attributes(name: &str, meta: &Metadata) -> FileAttributes {
    FileAttributes {
        hidden: name.starts_with('.'),
        system: false,
        readonly: meta.permissions().readonly(),
        archive: false,
        compressed: false,
        encrypted: false,
        reparse_point: false,
    }
}

/// Normalizes a path string for consistent comparison and SQLite storage.
pub(crate) fn canonical_db_path(path: &Path) -> String {
    let display = normalize_display_path(path);
    #[cfg(windows)]
    let normalized = display.replace('/', r"\");
    #[cfg(not(windows))]
    let normalized = display;

    // Trim trailing separators except for bare drive roots like `C:\` or `/`
    let is_bare_drive_root =
        normalized.len() == 3 && (normalized.ends_with(":\\") || normalized.ends_with(":/"));
    if normalized.len() > 1 && !is_bare_drive_root && normalized.ends_with(['\\', '/']) {
        normalized.trim_end_matches(['\\', '/']).to_string()
    } else {
        normalized
    }
}

/// Returns whether a directory metadata entry is safe to descend into (not a symlink or reparse point).
pub(crate) fn is_safe_traversable_dir(meta: &Metadata) -> bool {
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
            return false;
        }
    }
    true
}

/// Compares two path strings using OS-appropriate case sensitivity.
pub(crate) fn path_starts_with_dir(candidate: &str, dir: &str) -> bool {
    let sep = if cfg!(windows) { '\\' } else { '/' };
    let (cand_cmp, dir_cmp) = if cfg!(windows) {
        (candidate.to_lowercase(), dir.to_lowercase())
    } else {
        (candidate.to_string(), dir.to_string())
    };
    let dir_trimmed = dir_cmp.trim_end_matches(['\\', '/']);
    if cand_cmp == dir_trimmed || cand_cmp == dir_cmp {
        return true;
    }
    let mut prefix = String::with_capacity(dir_trimmed.len() + 1);
    prefix.push_str(dir_trimmed);
    prefix.push(sep);
    cand_cmp.starts_with(&prefix)
}

#[allow(dead_code)]
pub(crate) fn cmp_option_i64(a: Option<i64>, b: Option<i64>) -> Ordering {
    a.cmp(&b)
}
