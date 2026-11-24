use crate::types::{FileEntry, FileKind};
use chrono::{DateTime, Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// A clickable segment in the breadcrumb address bar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreadcrumbSegment {
    pub label: String,
    pub path: PathBuf,
}

/// Strips Windows extended-length prefixes (`\\?\` and `\\?\UNC\`) for clean UI display.
#[must_use]
pub fn normalize_display_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if let Some(unc) = raw.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(stripped) = raw.strip_prefix(r"\\?\") {
        stripped.to_string()
    } else {
        raw.into_owned()
    }
}

/// Converts a path into a Windows extended-length path (`\\?\...` or `\\?\UNC\...`)
/// when needed to safely handle paths longer than `MAX_PATH` (260 chars).
#[must_use]
pub fn to_extended_length_path(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if raw.starts_with(r"\\?\") || raw.starts_with(r"\\.\") {
        return path.to_path_buf();
    }
    if let Some(unc) = raw.strip_prefix(r"\\") {
        return PathBuf::from(format!(r"\\?\UNC\{unc}"));
    }
    if raw.len() >= 248 && path.is_absolute() {
        let cleaned = raw.replace('/', r"\");
        return PathBuf::from(format!(r"\\?\{cleaned}"));
    }
    path.to_path_buf()
}

/// Builds clickable breadcrumb segments from an absolute Windows or UNC path.
#[must_use]
pub fn build_breadcrumbs(path: &Path) -> Vec<BreadcrumbSegment> {
    let display = normalize_display_path(path);
    let normalized = PathBuf::from(&display);
    let mut segments = Vec::new();
    let mut acc = PathBuf::new();

    for component in normalized.components() {
        match component {
            Component::Prefix(prefix) => {
                let p = prefix.as_os_str().to_string_lossy().into_owned();
                acc.push(prefix.as_os_str());
                segments.push(BreadcrumbSegment {
                    label: p,
                    path: acc.clone(),
                });
            }
            Component::RootDir => {
                acc.push(component.as_os_str());
                if let Some(last) = segments.last_mut() {
                    last.path = acc.clone();
                } else {
                    segments.push(BreadcrumbSegment {
                        label: r"\".to_string(),
                        path: acc.clone(),
                    });
                }
            }
            Component::Normal(part) => {
                acc.push(part);
                segments.push(BreadcrumbSegment {
                    label: part.to_string_lossy().into_owned(),
                    path: acc.clone(),
                });
            }
            Component::CurDir | Component::ParentDir => {}
        }
    }

    segments
}

/// Formats a byte count into a clean Windows-friendly human-readable string.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit_idx = 0usize;
    while value >= 1024.0 && unit_idx < UNITS.len() - 1 {
        value /= 1024.0;
        unit_idx += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit_idx])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit_idx])
    } else {
        format!("{value:.2} {}", UNITS[unit_idx])
    }
}

/// Formats a UTC Unix timestamp (seconds) into local date/time display text.
#[must_use]
pub fn format_timestamp(unix_secs: Option<i64>) -> String {
    let Some(secs) = unix_secs else {
        return String::new();
    };
    let Some(utc): Option<DateTime<Utc>> = Utc.timestamp_opt(secs, 0).single() else {
        return String::new();
    };
    let local: DateTime<Local> = utc.with_timezone(&Local);
    local.format("%Y-%m-%d %H:%M").to_string()
}

/// Derives a concise Windows-style file type label from kind and extension.
#[must_use]
pub fn infer_type_label(kind: FileKind, extension: Option<&str>) -> String {
    match kind {
        FileKind::Directory => "File folder".to_string(),
        FileKind::Symlink => "Symbolic link".to_string(),
        FileKind::ReparsePoint => "Junction / Reparse".to_string(),
        FileKind::File | FileKind::Other => {
            match extension.map(|e| e.to_ascii_lowercase()).as_deref() {
                Some("rs") => "Rust Source".to_string(),
                Some("toml") => "TOML Config".to_string(),
                Some("json") => "JSON Document".to_string(),
                Some("md") => "Markdown".to_string(),
                Some("txt" | "log") => "Text Document".to_string(),
                Some("pdf") => "PDF Document".to_string(),
                Some("png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "ico" | "svg") => {
                    "Image".to_string()
                }
                Some("mp4" | "mkv" | "avi" | "mov" | "webm") => "Video".to_string(),
                Some("mp3" | "wav" | "flac" | "ogg" | "m4a") => "Audio".to_string(),
                Some("zip" | "7z" | "rar" | "tar" | "gz") => "Archive".to_string(),
                Some("exe" | "msi") => "Application".to_string(),
                Some("dll" | "sys") => "System File".to_string(),
                Some("ps1" | "bat" | "cmd") => "Script".to_string(),
                Some("slint") => "Slint UI".to_string(),
                Some(ext) if !ext.is_empty() => format!("{} File", ext.to_ascii_uppercase()),
                _ => "File".to_string(),
            }
        }
    }
}

/// Helper to populate `type_label` on a `FileEntry`.
pub fn populate_type_label(entry: &mut FileEntry) {
    entry.type_label = infer_type_label(entry.kind, entry.extension.as_deref());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_path_strips_extended_prefixes() {
        assert_eq!(
            normalize_display_path(Path::new(r"\\?\C:\Users\Test")),
            r"C:\Users\Test"
        );
        assert_eq!(
            normalize_display_path(Path::new(r"\\?\UNC\server\share\docs")),
            r"\\server\share\docs"
        );
    }

    #[test]
    fn extended_length_handles_long_and_unc_paths() {
        let unc = to_extended_length_path(Path::new(r"\\fileserver\share\dir"));
        assert_eq!(unc, PathBuf::from(r"\\?\UNC\fileserver\share\dir"));

        let long_seg = "a".repeat(260);
        let long = PathBuf::from(format!(r"C:\{long_seg}"));
        let ext = to_extended_length_path(&long);
        assert!(ext.to_string_lossy().starts_with(r"\\?\"));
    }

    #[test]
    fn breadcrumbs_split_windows_path() {
        let crumbs = build_breadcrumbs(Path::new(r"C:\Users\Alice\Documents"));
        assert_eq!(crumbs.len(), 4);
        assert_eq!(crumbs[0].label, "C:");
        assert_eq!(crumbs[0].path, PathBuf::from(r"C:\"));
        assert_eq!(crumbs[3].label, "Documents");
        assert_eq!(crumbs[3].path, PathBuf::from(r"C:\Users\Alice\Documents"));
    }

    #[test]
    fn format_bytes_human_readable() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.50 KB");
        assert_eq!(format_bytes(10 * 1024 * 1024), "10.0 MB");
    }
}
