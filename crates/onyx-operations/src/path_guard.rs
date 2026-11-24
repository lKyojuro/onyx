//! Path safety helpers: recursion detection, filename validation, conflict resolution,
//! and symlink/reparse point checks.

use crate::types::ConflictPolicy;
use onyx_core::{OnyxError, OnyxResult, normalize_display_path};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Resolves a path to a normalized comparable form (canonicalizing its longest existing
/// ancestor so relative segments, symlinks, and `\\?\` prefixes do not defeat checks).
fn normalize_for_comparison(path: &Path) -> PathBuf {
    let resolved = if let Ok(canon) = fs::canonicalize(path) {
        canon
    } else {
        // Walk up to the longest existing ancestor and re-attach remaining components.
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        let mut current = path;
        let base = loop {
            if let Ok(canon) = fs::canonicalize(current) {
                break canon;
            }
            if let Some(name) = current.file_name() {
                tail.push(name.to_os_string());
                if let Some(parent) = current.parent() {
                    current = parent;
                    continue;
                }
            }
            break current.to_path_buf();
        };
        let mut rebuilt = base;
        for part in tail.into_iter().rev() {
            rebuilt.push(part);
        }
        rebuilt
    };

    let display = normalize_display_path(&resolved);
    let mut normalized = PathBuf::new();
    for comp in Path::new(&display).components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn normalized_components_lower(path: &Path) -> Vec<String> {
    normalize_for_comparison(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect()
}

/// Returns `true` if `candidate` is the same path as `ancestor` or a descendant inside `ancestor`.
///
/// Used to refuse recursive operations such as copying or moving a folder into itself or a subfolder.
#[must_use]
pub fn is_same_or_descendant(ancestor: &Path, candidate: &Path) -> bool {
    let anc_parts = normalized_components_lower(ancestor);
    let cand_parts = normalized_components_lower(candidate);

    if anc_parts.is_empty() || cand_parts.len() < anc_parts.len() {
        return false;
    }

    cand_parts[..anc_parts.len()] == anc_parts[..]
}

/// Returns `true` if `a` and `b` resolve to the same case-insensitive normalized path.
#[must_use]
pub(crate) fn is_same_normalized_path(a: &Path, b: &Path) -> bool {
    normalized_components_lower(a) == normalized_components_lower(b)
}

/// Returns `true` if `a` and `b` reside on the same Windows drive/prefix (fast-rename eligible).
#[must_use]
pub(crate) fn is_same_volume(a: &Path, b: &Path) -> bool {
    let norm_a = normalize_for_comparison(a);
    let norm_b = normalize_for_comparison(b);
    match (norm_a.components().next(), norm_b.components().next()) {
        (Some(Component::Prefix(pa)), Some(Component::Prefix(pb))) => pa
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&pb.as_os_str().to_string_lossy()),
        _ => false,
    }
}

/// Validates a single file or directory name (not a full path).
pub fn validate_item_name(name: &str) -> OnyxResult<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(OnyxError::InvalidPath {
            message: "Name cannot be empty".to_string(),
        });
    }
    if trimmed == "." || trimmed == ".." {
        return Err(OnyxError::InvalidPath {
            message: format!("Invalid reserved name '{name}'"),
        });
    }
    if name.contains(['/', '\\', '\0', '<', '>', ':', '"', '|', '?', '*']) {
        return Err(OnyxError::InvalidPath {
            message: format!("Name '{name}' contains invalid Windows filename characters"),
        });
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(OnyxError::InvalidPath {
            message: format!("Windows names cannot end with a trailing dot or space: '{name}'"),
        });
    }
    Ok(())
}

/// Returns `true` if `path` exists as a file, directory, symlink, or broken symlink.
#[must_use]
pub(crate) fn path_exists_any(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Returns `true` if `metadata` represents a symlink or Windows reparse point (e.g. junction).
#[must_use]
pub(crate) fn is_reparse_or_symlink(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Returns `true` if a symlink/reparse point has the directory attribute on Windows
/// (where `symlink_metadata().is_dir()` returns `false`, yet `remove_dir` is required).
#[must_use]
pub(crate) fn is_dir_reparse(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
        (meta.file_attributes() & FILE_ATTRIBUTE_DIRECTORY) != 0
    }
    #[cfg(not(windows))]
    {
        meta.is_dir()
    }
}

/// Generates a non-conflicting sibling path using Windows Explorer style numbering:
/// `"report.pdf"` -> `"report (2).pdf"`, `"report (3).pdf"`, etc.,
/// and `"folder"` -> `"folder (2)"`, `"folder (3)"`, etc.
pub fn generate_auto_rename_path(desired: &Path) -> OnyxResult<PathBuf> {
    let parent = desired.parent().ok_or_else(|| OnyxError::InvalidPath {
        message: format!("Cannot derive parent directory for '{}'", desired.display()),
    })?;

    let is_dir_like = fs::symlink_metadata(desired).is_ok_and(|m| m.is_dir());
    let file_name = desired
        .file_name()
        .ok_or_else(|| OnyxError::InvalidPath {
            message: format!("Missing file name in '{}'", desired.display()),
        })?
        .to_string_lossy();

    let (stem, ext) = if is_dir_like {
        (file_name.into_owned(), None)
    } else {
        let stem = desired
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| file_name.into_owned());
        let ext = desired
            .extension()
            .map(|e| e.to_string_lossy().into_owned());
        (stem, ext)
    };

    for counter in 2u32..=100_000 {
        let candidate_name = match &ext {
            Some(extension) => format!("{stem} ({counter}).{extension}"),
            None => format!("{stem} ({counter})"),
        };
        let candidate = parent.join(candidate_name);
        if !path_exists_any(&candidate) {
            return Ok(candidate);
        }
    }

    Err(OnyxError::OperationFailed {
        message: format!(
            "Could not generate unique auto-rename target for '{}'",
            desired.display()
        ),
    })
}

/// Resolution produced by applying a [`ConflictPolicy`] to a desired destination path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConflictResolution {
    /// Proceed using this destination path (does not exist yet).
    Proceed(PathBuf),
    /// Overwrite the existing item at this destination path.
    Overwrite(PathBuf),
    /// Skip the item.
    Skip,
}

/// Resolves whether a destination path can be used, overwritten, auto-renamed, skipped, or failed.
pub(crate) fn resolve_conflict(
    source: &Path,
    desired: &Path,
    policy: ConflictPolicy,
    allow_case_only_same_path: bool,
) -> OnyxResult<ConflictResolution> {
    if !path_exists_any(desired) {
        return Ok(ConflictResolution::Proceed(desired.to_path_buf()));
    }

    if is_same_normalized_path(source, desired) {
        if allow_case_only_same_path && source.file_name() != desired.file_name() {
            return Ok(ConflictResolution::Proceed(desired.to_path_buf()));
        }
        return match policy {
            ConflictPolicy::AutoRename => {
                let renamed = generate_auto_rename_path(desired)?;
                Ok(ConflictResolution::Proceed(renamed))
            }
            ConflictPolicy::Skip | ConflictPolicy::Overwrite => Ok(ConflictResolution::Skip),
            ConflictPolicy::Fail => Err(OnyxError::AlreadyExists {
                path: desired.to_path_buf(),
            }),
        };
    }

    match policy {
        ConflictPolicy::Skip => Ok(ConflictResolution::Skip),
        ConflictPolicy::Overwrite => Ok(ConflictResolution::Overwrite(desired.to_path_buf())),
        ConflictPolicy::Fail => Err(OnyxError::AlreadyExists {
            path: desired.to_path_buf(),
        }),
        ConflictPolicy::AutoRename => {
            let renamed = generate_auto_rename_path(desired)?;
            Ok(ConflictResolution::Proceed(renamed))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn recursion_guard_detects_same_and_descendant_paths() {
        let temp = tempfile::TempDir::new().unwrap();
        let parent = temp.path().join("Parent");
        let child = parent.join("Child");
        let sibling = temp.path().join("Parent2");
        fs::create_dir_all(&child).unwrap();
        fs::create_dir_all(&sibling).unwrap();

        assert!(is_same_or_descendant(&parent, &parent));
        assert!(is_same_or_descendant(&parent, &child));
        assert!(is_same_or_descendant(
            &parent,
            &child.join("nonexistent_deep")
        ));
        assert!(!is_same_or_descendant(&parent, &sibling));
        assert!(!is_same_or_descendant(&child, &parent));
    }

    #[test]
    fn auto_rename_generates_numbered_variants() {
        let temp = tempfile::TempDir::new().unwrap();
        let f1 = temp.path().join("notes.txt");
        fs::write(&f1, b"1").unwrap();

        let next = generate_auto_rename_path(&f1).unwrap();
        assert_eq!(next, temp.path().join("notes (2).txt"));

        fs::write(&next, b"2").unwrap();
        let next2 = generate_auto_rename_path(&f1).unwrap();
        assert_eq!(next2, temp.path().join("notes (3).txt"));

        let dir = temp.path().join("Project");
        fs::create_dir(&dir).unwrap();
        let next_dir = generate_auto_rename_path(&dir).unwrap();
        assert_eq!(next_dir, temp.path().join("Project (2)"));
    }

    #[test]
    fn validate_item_name_rejects_invalid_names() {
        assert!(validate_item_name("").is_err());
        assert!(validate_item_name("   ").is_err());
        assert!(validate_item_name(".").is_err());
        assert!(validate_item_name("..").is_err());
        assert!(validate_item_name("a/b").is_err());
        assert!(validate_item_name(r"a\b").is_err());
        assert!(validate_item_name("bad:name").is_err());
        assert!(validate_item_name("trailing.").is_err());
        assert!(validate_item_name("valid_file-1.rs").is_ok());
    }
}
