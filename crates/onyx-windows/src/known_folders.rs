//! Windows Known Folders resolution via `SHGetKnownFolderPath`.

use crate::com::ComGuard;
use onyx_core::{KnownFolder, KnownFolderKind};
use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music, FOLDERID_Pictures,
    FOLDERID_Profile, FOLDERID_Videos, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};
use windows::core::{GUID, PWSTR};

/// RAII guard that frees a COM task-allocated `PWSTR` via `CoTaskMemFree`.
struct CoTaskMemStrGuard(PWSTR);

impl CoTaskMemStrGuard {
    fn to_path_buf(&self) -> Option<PathBuf> {
        if self.0.is_null() {
            return None;
        }
        // SAFETY: `self.0` is a valid non-null, null-terminated UTF-16 string returned by
        // `SHGetKnownFolderPath` and remains live until `Drop`.
        let slice = unsafe { self.0.as_wide() };
        Some(PathBuf::from(OsString::from_wide(slice)))
    }
}

impl Drop for CoTaskMemStrGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` was allocated by the COM task allocator (`SHGetKnownFolderPath`)
            // and is freed exactly once here.
            unsafe {
                CoTaskMemFree(Some(self.0.0.cast::<c_void>()));
            }
        }
    }
}

fn resolve_known_folder(folder_id: &GUID) -> Option<PathBuf> {
    // SAFETY: `folder_id` points to a valid `KNOWNFOLDERID` GUID. The returned `PWSTR` is
    // immediately wrapped in `CoTaskMemStrGuard` to guarantee `CoTaskMemFree` on drop.
    let raw = unsafe { SHGetKnownFolderPath(folder_id, KF_FLAG_DEFAULT, None) }.ok()?;
    let guard = CoTaskMemStrGuard(raw);
    guard.to_path_buf()
}

/// Enumerates standard Windows user known folders (`Home`, `Desktop`, `Downloads`,
/// `Documents`, `Pictures`, `Music`, `Videos`).
#[must_use]
pub fn enumerate_known_folders() -> Vec<KnownFolder> {
    let _com = ComGuard::init_sta().ok();

    let specs: [(KnownFolderKind, &str, &GUID); 7] = [
        (KnownFolderKind::Home, "Home", &FOLDERID_Profile),
        (KnownFolderKind::Desktop, "Desktop", &FOLDERID_Desktop),
        (KnownFolderKind::Downloads, "Downloads", &FOLDERID_Downloads),
        (KnownFolderKind::Documents, "Documents", &FOLDERID_Documents),
        (KnownFolderKind::Pictures, "Pictures", &FOLDERID_Pictures),
        (KnownFolderKind::Music, "Music", &FOLDERID_Music),
        (KnownFolderKind::Videos, "Videos", &FOLDERID_Videos),
    ];

    let mut folders = Vec::with_capacity(specs.len());
    for (kind, name, guid) in specs {
        if let Some(path) = resolve_known_folder(guid) {
            folders.push(KnownFolder {
                kind,
                name: name.to_string(),
                path,
            });
        } else {
            tracing::debug!(?kind, "Known folder could not be resolved");
        }
    }

    folders
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn enumerate_known_folders_resolves_profile() {
        let folders = enumerate_known_folders();
        assert!(!folders.is_empty());
        let home = folders.iter().find(|f| f.kind == KnownFolderKind::Home);
        assert!(
            home.is_some(),
            "FOLDERID_Profile (Home) should resolve on Windows"
        );
        assert!(home.unwrap().path.exists());
    }
}
