use crate::types::FileEntry;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// Supported file list sort columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum SortColumn {
    #[default]
    Name,
    Modified,
    Kind,
    Size,
}

/// Sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum SortDirection {
    #[default]
    Ascending,
    Descending,
}

impl SortDirection {
    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::Ascending => Self::Descending,
            Self::Descending => Self::Ascending,
        }
    }
}

/// Full sorting specification for a directory view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortSpec {
    pub column: SortColumn,
    pub direction: SortDirection,
    pub folders_first: bool,
}

impl Default for SortSpec {
    fn default() -> Self {
        Self {
            column: SortColumn::Name,
            direction: SortDirection::Ascending,
            folders_first: true,
        }
    }
}

/// Windows Explorer-style natural, case-insensitive string comparison.
/// Compares embedded ASCII digit runs numerically (`file2.txt` < `file10.txt`)
/// while remaining allocation-free and safe on arbitrary Unicode strings.
#[must_use]
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut a_chars = a.chars().peekable();
    let mut b_chars = b.chars().peekable();

    while let (Some(&ac), Some(&bc)) = (a_chars.peek(), b_chars.peek()) {
        if ac.is_ascii_digit() && bc.is_ascii_digit() {
            let mut a_leading_zeros = 0usize;
            while a_chars.peek() == Some(&'0') {
                a_chars.next();
                a_leading_zeros += 1;
            }
            let mut b_leading_zeros = 0usize;
            while b_chars.peek() == Some(&'0') {
                b_chars.next();
                b_leading_zeros += 1;
            }

            let mut a_digits = String::new();
            while let Some(&c) = a_chars.peek() {
                if c.is_ascii_digit() {
                    a_digits.push(c);
                    a_chars.next();
                } else {
                    break;
                }
            }
            let mut b_digits = String::new();
            while let Some(&c) = b_chars.peek() {
                if c.is_ascii_digit() {
                    b_digits.push(c);
                    b_chars.next();
                } else {
                    break;
                }
            }

            // Compare significant digit lengths first, then lexicographically (handles arbitrarily big numbers).
            let ord = a_digits
                .len()
                .cmp(&b_digits.len())
                .then_with(|| a_digits.cmp(&b_digits));
            if ord != Ordering::Equal {
                return ord;
            }
            // Tie-break on leading zero count (more leading zeros sorts first, like StrCmpLogicalW).
            let zero_ord = b_leading_zeros.cmp(&a_leading_zeros);
            if zero_ord != Ordering::Equal {
                return zero_ord;
            }
        } else {
            a_chars.next();
            b_chars.next();
            let a_lower = ac.to_lowercase();
            let b_lower = bc.to_lowercase();
            let ord = a_lower.cmp(b_lower);
            if ord != Ordering::Equal {
                return ord;
            }
        }
    }

    match (a_chars.peek(), b_chars.peek()) {
        (None, None) => a.cmp(b),
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(_), Some(_)) => Ordering::Equal,
    }
}

/// Compare two `FileEntry` values according to `SortSpec`.
#[must_use]
pub fn compare_entries(a: &FileEntry, b: &FileEntry, spec: SortSpec) -> Ordering {
    if spec.folders_first {
        match (a.is_dir(), b.is_dir()) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
    }

    let base = match spec.column {
        SortColumn::Name => natural_cmp(&a.name, &b.name),
        SortColumn::Modified => a
            .modified
            .unwrap_or(0)
            .cmp(&b.modified.unwrap_or(0))
            .then_with(|| natural_cmp(&a.name, &b.name)),
        SortColumn::Kind => {
            natural_cmp(&a.type_label, &b.type_label).then_with(|| natural_cmp(&a.name, &b.name))
        }
        SortColumn::Size => a
            .size
            .cmp(&b.size)
            .then_with(|| natural_cmp(&a.name, &b.name)),
    };

    match spec.direction {
        SortDirection::Ascending => base,
        SortDirection::Descending => base.reverse(),
    }
}

/// Sort a slice of `FileEntry` in-place using `SortSpec`.
pub fn sort_entries(entries: &mut [FileEntry], spec: SortSpec) {
    entries.sort_by(|a, b| compare_entries(a, b, spec));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FileAttributes, FileKind};
    use std::path::PathBuf;

    #[test]
    fn natural_sort_orders_numeric_runs_correctly() {
        let mut names = vec![
            "file10.txt",
            "file2.txt",
            "File1.txt",
            "file20.txt",
            "file3.txt",
        ];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            vec![
                "File1.txt",
                "file2.txt",
                "file3.txt",
                "file10.txt",
                "file20.txt"
            ]
        );
    }

    #[test]
    fn folders_first_sorting() {
        let make = |name: &str, kind: FileKind, size: u64| FileEntry {
            path: PathBuf::from(name),
            name: name.to_string(),
            extension: None,
            kind,
            size,
            modified: Some(100),
            created: Some(100),
            attributes: FileAttributes::default(),
            is_symlink: false,
            symlink_target: None,
            type_label: String::new(),
        };
        let mut entries = vec![
            make("alpha.txt", FileKind::File, 10),
            make("zeta", FileKind::Directory, 0),
        ];
        sort_entries(&mut entries, SortSpec::default());
        assert_eq!(entries[0].name, "zeta");
        assert_eq!(entries[1].name, "alpha.txt");
    }
}
