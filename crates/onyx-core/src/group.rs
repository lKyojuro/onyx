//! Deterministic, UI-agnostic "Group By" engine for `FileEntry` collections.

use crate::path::infer_type_label;
use crate::sort::natural_cmp;
use crate::types::{FileEntry, FileKind};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

const TINY_MAX_BYTES: u64 = 10 * 1024;
const SMALL_MAX_BYTES: u64 = 1024 * 1024;
const MEDIUM_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// Supported directory grouping modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum GroupByMode {
    #[default]
    None,
    Kind,
    Extension,
    ModifiedDate,
    Size,
}

/// Deterministic modification-date bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum DateBucket {
    Today,
    Yesterday,
    ThisWeek,
    ThisMonth,
    Older,
    Unknown,
}

impl DateBucket {
    /// Machine-readable stable key for this bucket.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Today => "date:today",
            Self::Yesterday => "date:yesterday",
            Self::ThisWeek => "date:this_week",
            Self::ThisMonth => "date:this_month",
            Self::Older => "date:older",
            Self::Unknown => "date:unknown",
        }
    }

    /// Human-readable display header for this bucket.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Today => "Today",
            Self::Yesterday => "Yesterday",
            Self::ThisWeek => "This Week",
            Self::ThisMonth => "This Month",
            Self::Older => "Older",
            Self::Unknown => "Unknown",
        }
    }
}

/// Deterministic file-size bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SizeBucket {
    Folders,
    Empty,
    Tiny,
    Small,
    Medium,
    Large,
}

impl SizeBucket {
    /// Machine-readable stable key for this bucket.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Folders => "size:folders",
            Self::Empty => "size:empty",
            Self::Tiny => "size:tiny",
            Self::Small => "size:small",
            Self::Medium => "size:medium",
            Self::Large => "size:large",
        }
    }

    /// Human-readable display header for this bucket.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Folders => "Folders",
            Self::Empty => "Empty",
            Self::Tiny => "Tiny (< 10 KB)",
            Self::Small => "Small (< 1 MB)",
            Self::Medium => "Medium (< 100 MB)",
            Self::Large => "Large (>= 100 MB)",
        }
    }
}

/// A deterministic grouped section of `FileEntry` items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryGroup {
    /// Stable, machine-readable group key (e.g. `"kind:folders"`, `"ext:rs"`, `"date:today"`).
    pub key: String,
    /// Human-readable section header label (e.g. `"Folders"`, `".RS"`, `"Today"`).
    pub label: String,
    /// Entries belonging to this group, preserving their relative input order.
    pub entries: Vec<FileEntry>,
}

impl EntryGroup {
    /// Number of entries in this group.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the group contains no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Classifies a UTC Unix timestamp (seconds) into a `DateBucket` relative to `now_utc_secs`.
#[must_use]
pub fn classify_date_bucket(modified_utc_secs: Option<i64>, now_utc_secs: i64) -> DateBucket {
    let Some(modified_secs) = modified_utc_secs else {
        return DateBucket::Unknown;
    };
    let Some(now_dt) = DateTime::from_timestamp(now_utc_secs, 0) else {
        return DateBucket::Unknown;
    };
    let Some(entry_dt) = DateTime::from_timestamp(modified_secs, 0) else {
        return DateBucket::Unknown;
    };

    let days_ago = now_dt
        .date_naive()
        .signed_duration_since(entry_dt.date_naive())
        .num_days();

    match days_ago {
        ..=0 => DateBucket::Today,
        1 => DateBucket::Yesterday,
        2..=6 => DateBucket::ThisWeek,
        7..=29 => DateBucket::ThisMonth,
        _ => DateBucket::Older,
    }
}

/// Classifies a `FileEntry` into a `SizeBucket`.
///
/// Directories are placed in `SizeBucket::Folders`; files are bucketed by byte size:
/// `Empty (0 B)`, `Tiny (< 10 KB)`, `Small (< 1 MB)`, `Medium (< 100 MB)`, `Large (>= 100 MB)`.
#[must_use]
pub fn classify_size_bucket(entry: &FileEntry) -> SizeBucket {
    if entry.is_dir() {
        return SizeBucket::Folders;
    }
    match entry.size {
        0 => SizeBucket::Empty,
        1..TINY_MAX_BYTES => SizeBucket::Tiny,
        TINY_MAX_BYTES..SMALL_MAX_BYTES => SizeBucket::Small,
        SMALL_MAX_BYTES..MEDIUM_MAX_BYTES => SizeBucket::Medium,
        _ => SizeBucket::Large,
    }
}

fn kind_group_descriptor(entry: &FileEntry) -> (u8, String, String) {
    match entry.kind {
        FileKind::Directory => (0, "kind:folders".to_string(), "Folders".to_string()),
        FileKind::Symlink => (1, "kind:symlinks".to_string(), "Symbolic Links".to_string()),
        FileKind::ReparsePoint => (
            2,
            "kind:reparse".to_string(),
            "Junction / Reparse".to_string(),
        ),
        FileKind::File => {
            let label = if entry.type_label.trim().is_empty() {
                infer_type_label(entry.kind, entry.extension.as_deref())
            } else {
                entry.type_label.trim().to_string()
            };
            let key = format!("kind:file:{}", label.to_lowercase());
            (3, key, label)
        }
        FileKind::Other => (4, "kind:other".to_string(), "Other".to_string()),
    }
}

fn extension_group_descriptor(entry: &FileEntry) -> (u8, String, String) {
    if entry.is_dir() {
        return (0, "ext:folders".to_string(), "Folders".to_string());
    }

    let raw_ext = entry
        .extension
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_start_matches('.'))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            Path::new(&entry.name)
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty())
        });

    match raw_ext {
        Some(ext) => {
            let lower = ext.to_lowercase();
            let upper = ext.to_uppercase();
            (1, format!("ext:{lower}"), format!(".{upper}"))
        }
        None => (2, "ext:none".to_string(), "No Extension".to_string()),
    }
}

/// Groups `entries` according to `mode` using the current UTC wall-clock time.
#[must_use]
pub fn group_entries(entries: &[FileEntry], mode: GroupByMode) -> Vec<EntryGroup> {
    group_entries_at(entries, mode, Utc::now().timestamp())
}

/// Groups `entries` according to `mode` using an explicit `now_utc_secs` reference timestamp.
///
/// - Empty input always returns an empty `Vec`.
/// - Group ordering is completely deterministic.
/// - Relative order of entries within each group is preserved.
#[must_use]
pub fn group_entries_at(
    entries: &[FileEntry],
    mode: GroupByMode,
    now_utc_secs: i64,
) -> Vec<EntryGroup> {
    if entries.is_empty() {
        return Vec::new();
    }

    if mode == GroupByMode::None {
        return vec![EntryGroup {
            key: "all".to_string(),
            label: "All Items".to_string(),
            entries: entries.to_vec(),
        }];
    }

    // Store (primary_rank, EntryGroup) keyed by stable group key.
    let mut index_by_key: HashMap<String, usize> = HashMap::new();
    let mut buckets: Vec<(u8, EntryGroup)> = Vec::new();

    for entry in entries {
        let (rank, key, label) = match mode {
            GroupByMode::None => (0, "all".to_string(), "All Items".to_string()),
            GroupByMode::Kind => kind_group_descriptor(entry),
            GroupByMode::Extension => extension_group_descriptor(entry),
            GroupByMode::ModifiedDate => {
                let bucket = classify_date_bucket(entry.modified, now_utc_secs);
                (
                    bucket as u8,
                    bucket.key().to_string(),
                    bucket.label().to_string(),
                )
            }
            GroupByMode::Size => {
                let bucket = classify_size_bucket(entry);
                (
                    bucket as u8,
                    bucket.key().to_string(),
                    bucket.label().to_string(),
                )
            }
        };

        if let Some(&idx) = index_by_key.get(&key) {
            buckets[idx].1.entries.push(entry.clone());
        } else {
            let idx = buckets.len();
            index_by_key.insert(key.clone(), idx);
            buckets.push((
                rank,
                EntryGroup {
                    key,
                    label,
                    entries: vec![entry.clone()],
                },
            ));
        }
    }

    buckets.sort_by(|(rank_a, group_a), (rank_b, group_b)| {
        rank_a
            .cmp(rank_b)
            .then_with(|| natural_cmp(&group_a.label, &group_b.label))
            .then_with(|| group_a.key.cmp(&group_b.key))
    });

    buckets.into_iter().map(|(_, group)| group).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FileAttributes;
    use std::path::PathBuf;

    // 2026-06-15 12:00:00 UTC
    const NOW_UTC: i64 = 1_781_524_800;
    const DAY_SECS: i64 = 86_400;

    fn make_entry(
        name: &str,
        kind: FileKind,
        ext: Option<&str>,
        size: u64,
        modified: Option<i64>,
        type_label: &str,
    ) -> FileEntry {
        FileEntry {
            path: PathBuf::from(name),
            name: name.to_string(),
            extension: ext.map(str::to_string),
            kind,
            size,
            modified,
            created: modified,
            attributes: FileAttributes::default(),
            is_symlink: matches!(kind, FileKind::Symlink),
            symlink_target: None,
            type_label: type_label.to_string(),
        }
    }

    #[test]
    fn empty_entries_return_no_groups_for_any_mode() {
        for mode in [
            GroupByMode::None,
            GroupByMode::Kind,
            GroupByMode::Extension,
            GroupByMode::ModifiedDate,
            GroupByMode::Size,
        ] {
            assert!(group_entries_at(&[], mode, NOW_UTC).is_empty());
        }
    }

    #[test]
    fn group_by_none_returns_single_all_items_group() {
        let entries = vec![
            make_entry("b.txt", FileKind::File, Some("txt"), 10, Some(NOW_UTC), ""),
            make_entry("a.rs", FileKind::File, Some("rs"), 20, Some(NOW_UTC), ""),
        ];
        let groups = group_entries_at(&entries, GroupByMode::None, NOW_UTC);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].key, "all");
        assert_eq!(groups[0].label, "All Items");
        assert_eq!(groups[0].len(), 2);
        assert_eq!(groups[0].entries[0].name, "b.txt");
        assert_eq!(groups[0].entries[1].name, "a.rs");
    }

    #[test]
    fn group_by_kind_orders_folders_first_then_types_deterministically() {
        let entries = vec![
            make_entry(
                "main.rs",
                FileKind::File,
                Some("rs"),
                100,
                Some(NOW_UTC),
                "Rust Source",
            ),
            make_entry("docs", FileKind::Directory, None, 0, Some(NOW_UTC), ""),
            make_entry("link", FileKind::Symlink, None, 0, Some(NOW_UTC), ""),
            make_entry(
                "photo.png",
                FileKind::File,
                Some("png"),
                500,
                Some(NOW_UTC),
                "",
            ),
            make_entry("lib.rs", FileKind::File, Some("rs"), 200, Some(NOW_UTC), ""),
            make_entry("weird", FileKind::Other, None, 0, Some(NOW_UTC), ""),
        ];

        let groups = group_entries_at(&entries, GroupByMode::Kind, NOW_UTC);
        let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["Folders", "Symbolic Links", "Image", "Rust Source", "Other"]
        );

        let Some(rust_group) = groups.iter().find(|g| g.label == "Rust Source") else {
            panic!("expected Rust Source group");
        };
        assert_eq!(rust_group.len(), 2);
        assert_eq!(rust_group.entries[0].name, "main.rs");
        assert_eq!(rust_group.entries[1].name, "lib.rs");
    }

    #[test]
    fn group_by_extension_normalizes_case_and_handles_missing_extensions() {
        let entries = vec![
            make_entry("Makefile", FileKind::File, None, 100, Some(NOW_UTC), ""),
            make_entry(
                "README.MD",
                FileKind::File,
                Some("MD"),
                100,
                Some(NOW_UTC),
                "",
            ),
            make_entry("src", FileKind::Directory, None, 0, Some(NOW_UTC), ""),
            make_entry(
                "notes.md",
                FileKind::File,
                Some(".md"),
                100,
                Some(NOW_UTC),
                "",
            ),
            make_entry("app.rs", FileKind::File, None, 100, Some(NOW_UTC), ""),
        ];

        let groups = group_entries_at(&entries, GroupByMode::Extension, NOW_UTC);
        let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, vec!["Folders", ".MD", ".RS", "No Extension"]);

        let Some(md_group) = groups.iter().find(|g| g.key == "ext:md") else {
            panic!("expected ext:md group");
        };
        assert_eq!(md_group.len(), 2);
        assert_eq!(md_group.entries[0].name, "README.MD");
        assert_eq!(md_group.entries[1].name, "notes.md");
    }

    #[test]
    fn group_by_modified_date_buckets_all_ranges_deterministically() {
        let entries = vec![
            make_entry("no_date.txt", FileKind::File, Some("txt"), 10, None, ""),
            make_entry(
                "ancient.txt",
                FileKind::File,
                Some("txt"),
                10,
                Some(NOW_UTC - 60 * DAY_SECS),
                "",
            ),
            make_entry(
                "month.txt",
                FileKind::File,
                Some("txt"),
                10,
                Some(NOW_UTC - 14 * DAY_SECS),
                "",
            ),
            make_entry(
                "week.txt",
                FileKind::File,
                Some("txt"),
                10,
                Some(NOW_UTC - 3 * DAY_SECS),
                "",
            ),
            make_entry(
                "yesterday.txt",
                FileKind::File,
                Some("txt"),
                10,
                Some(NOW_UTC - DAY_SECS),
                "",
            ),
            make_entry(
                "today.txt",
                FileKind::File,
                Some("txt"),
                10,
                Some(NOW_UTC - 60),
                "",
            ),
            make_entry(
                "future.txt",
                FileKind::File,
                Some("txt"),
                10,
                Some(NOW_UTC + DAY_SECS),
                "",
            ),
        ];

        let groups = group_entries_at(&entries, GroupByMode::ModifiedDate, NOW_UTC);
        let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Today",
                "Yesterday",
                "This Week",
                "This Month",
                "Older",
                "Unknown",
            ]
        );
        assert_eq!(groups[0].len(), 2); // today.txt + future.txt
        assert_eq!(groups[1].entries[0].name, "yesterday.txt");
        assert_eq!(groups[2].entries[0].name, "week.txt");
        assert_eq!(groups[3].entries[0].name, "month.txt");
        assert_eq!(groups[4].entries[0].name, "ancient.txt");
        assert_eq!(groups[5].entries[0].name, "no_date.txt");
    }

    #[test]
    fn group_by_size_buckets_all_thresholds_accurately() {
        let entries = vec![
            make_entry(
                "iso.img",
                FileKind::File,
                Some("img"),
                100 * 1024 * 1024,
                Some(NOW_UTC),
                "",
            ),
            make_entry(
                "video.mp4",
                FileKind::File,
                Some("mp4"),
                1024 * 1024,
                Some(NOW_UTC),
                "",
            ),
            make_entry(
                "doc.pdf",
                FileKind::File,
                Some("pdf"),
                10 * 1024,
                Some(NOW_UTC),
                "",
            ),
            make_entry(
                "tiny.txt",
                FileKind::File,
                Some("txt"),
                10 * 1024 - 1,
                Some(NOW_UTC),
                "",
            ),
            make_entry(
                "empty.txt",
                FileKind::File,
                Some("txt"),
                0,
                Some(NOW_UTC),
                "",
            ),
            make_entry("folder", FileKind::Directory, None, 0, Some(NOW_UTC), ""),
        ];

        let groups = group_entries_at(&entries, GroupByMode::Size, NOW_UTC);
        let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Folders",
                "Empty",
                "Tiny (< 10 KB)",
                "Small (< 1 MB)",
                "Medium (< 100 MB)",
                "Large (>= 100 MB)",
            ]
        );
        assert_eq!(groups[0].entries[0].name, "folder");
        assert_eq!(groups[1].entries[0].name, "empty.txt");
        assert_eq!(groups[2].entries[0].name, "tiny.txt");
        assert_eq!(groups[3].entries[0].name, "doc.pdf");
        assert_eq!(groups[4].entries[0].name, "video.mp4");
        assert_eq!(groups[5].entries[0].name, "iso.img");
    }
}
