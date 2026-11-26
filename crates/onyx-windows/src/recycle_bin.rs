//! Windows Recycle Bin enumeration through the Shell namespace.
//!
//! The Recycle Bin is a Shell namespace folder (`FOLDERID_RecycleBinFolder`), not a
//! directory on disk, so `std::fs` cannot list it. The code below binds the Shell
//! folder, walks its item identifiers and reads the properties the Explorer Recycle
//! Bin column set shows: display name, original location, size and deletion time.

use crate::com::ComGuard;
use onyx_core::{OnyxError, OnyxResult};
use std::ffi::c_void;
use std::path::PathBuf;
use windows::Win32::Foundation::{FILETIME, HWND, PROPERTYKEY};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::SystemServices::SFGAO_FOLDER;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_SFObject, FOLDERID_RecycleBinFolder, IEnumIDList, IShellFolder, IShellItem, IShellItem2,
    KF_FLAG_DEFAULT, SHCONTF_FOLDERS, SHCONTF_INCLUDEHIDDEN, SHCONTF_NONFOLDERS,
    SHCreateItemWithParent, SHGetKnownFolderItem, SIGDN, SIGDN_DESKTOPABSOLUTEPARSING,
    SIGDN_NORMALDISPLAY,
};
use windows::core::{GUID, PWSTR};

/// `PKEY_Size` (`System.Size`) from `propkey.h`.
const PKEY_SIZE: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xb725_f130_47ef_101a_a5f1_0260_8c9e_ebac),
    pid: 12,
};

/// `PKEY_RecycleBin_DeletedFrom` (`System.Recycle.DeletedFrom`) from `propkey.h`:
/// the folder the item was deleted from.
const PKEY_RECYCLE_DELETED_FROM: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9b17_4b33_40ff_11d2_a27e_00c0_4fc3_0871),
    pid: 2,
};

/// `PKEY_RecycleBin_DateDeleted` (`System.Recycle.DateDeleted`) from `propkey.h`:
/// the deletion time as `VT_FILETIME`.
const PKEY_RECYCLE_DATE_DELETED: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9b17_4b33_40ff_11d2_a27e_00c0_4fc3_0871),
    pid: 3,
};

/// Seconds between the Windows file-time epoch (1601-01-01) and the Unix epoch.
const FILETIME_UNIX_EPOCH_OFFSET_SECS: i64 = 11_644_473_600;

/// A `FILETIME` counts 100-nanosecond ticks.
const FILETIME_TICKS_PER_SEC: u64 = 10_000_000;

/// One item the Windows Recycle Bin holds.
///
/// The UI builds a [`onyx_core::FileEntry`]-shaped row from this, keeping
/// [`Self::original_path`] aside because a recycled item has no live path of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecycleBinItem {
    /// Name the Shell shows for the item: the original file name without the
    /// Recycle Bin's internal `$R` prefix.
    pub name: String,
    /// Path of the item inside the Recycle Bin Shell namespace
    /// (`::{645FF040-5081-101B-9F08-00AA002F954E}\$RABCDEF.txt`). Unique per item,
    /// so it works as list identity and icon cache key.
    pub shell_path: PathBuf,
    /// Full original location the item is restored to, when the Shell reports it.
    pub original_path: Option<PathBuf>,
    /// Size in bytes the Shell reports, `0` when unknown.
    pub size: u64,
    /// Deletion time as Unix seconds (UTC), when the Shell reports it.
    pub deleted_at: Option<i64>,
    /// Whether the item was a folder before it was deleted.
    pub is_dir: bool,
}

/// Raw property values read from one Shell item.
///
/// Kept separate from the COM calls so the mapping onto [`RecycleBinItem`] stays
/// testable without a populated Recycle Bin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ShellItemFacts {
    name: String,
    shell_path: PathBuf,
    deleted_from: Option<String>,
    size: u64,
    deleted_at: Option<i64>,
    is_dir: bool,
}

impl ShellItemFacts {
    /// Maps the raw Shell values onto a public item, dropping what cannot be named.
    fn into_item(self) -> Option<RecycleBinItem> {
        let reported = self.name.trim();
        if reported.is_empty() || self.shell_path.as_os_str().is_empty() {
            return None;
        }
        // Most builds report the plain file name; some report the full original path.
        let name = PathBuf::from(reported)
            .file_name()
            .map_or_else(
                || reported.to_string(),
                |last| last.to_string_lossy().into_owned(),
            )
            .trim()
            .to_string();
        if name.is_empty() {
            return None;
        }
        Some(RecycleBinItem {
            original_path: original_path_from(self.deleted_from.as_deref(), reported, &name),
            name,
            shell_path: self.shell_path,
            size: self.size,
            deleted_at: self.deleted_at,
            is_dir: self.is_dir,
        })
    }
}

/// Resolves the original location of a recycled item.
///
/// `reported_name` is the Shell's display name: the plain file name on most builds,
/// the full original path on others. `deleted_from` (`System.Recycle.DeletedFrom`)
/// carries the folder the item came from, and the full path on those same builds.
/// A path that already ends in the name must not get the name a second time.
fn original_path_from(
    deleted_from: Option<&str>,
    reported_name: &str,
    name: &str,
) -> Option<PathBuf> {
    let reported_path = PathBuf::from(reported_name);
    if reported_path
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty())
    {
        return Some(reported_path);
    }
    let folder = deleted_from?.trim();
    if folder.is_empty() {
        return None;
    }
    let folder_path = PathBuf::from(folder);
    let already_full = folder_path
        .file_name()
        .is_some_and(|last| last.to_string_lossy().eq_ignore_ascii_case(name));
    Some(if already_full {
        folder_path
    } else {
        folder_path.join(name)
    })
}

/// Converts a Shell file time into Unix seconds (UTC).
///
/// Returns `None` for the zero value the Shell uses for items without a timestamp.
fn filetime_to_unix(file_time: FILETIME) -> Option<i64> {
    let ticks = (u64::from(file_time.dwHighDateTime) << 32) | u64::from(file_time.dwLowDateTime);
    if ticks == 0 {
        return None;
    }
    let seconds = i64::try_from(ticks / FILETIME_TICKS_PER_SEC).ok()?;
    Some(seconds - FILETIME_UNIX_EPOCH_OFFSET_SECS)
}

/// RAII guard that frees a COM task-allocated `PWSTR` via `CoTaskMemFree`.
struct CoTaskMemStrGuard(PWSTR);

impl CoTaskMemStrGuard {
    /// Reads the guarded string; `None` when the Shell returned a null pointer.
    fn to_text(&self) -> Option<String> {
        if self.0.is_null() {
            return None;
        }
        // SAFETY: `self.0` is a valid non-null, null-terminated UTF-16 string owned by
        // this guard and freed exactly once on drop.
        unsafe { self.0.to_string() }.ok()
    }
}

impl Drop for CoTaskMemStrGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` was allocated by the COM task allocator through one of the
            // Shell property/display-name calls and is freed exactly once here.
            unsafe { CoTaskMemFree(Some(self.0.0.cast::<c_void>())) };
        }
    }
}

/// RAII guard that frees a Shell-allocated child item identifier.
struct PidlGuard(*mut ITEMIDLIST);

impl Drop for PidlGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `IEnumIDList::Next` hands out item identifiers allocated by the
            // Shell's COM task allocator; this guard owns exactly one of them.
            unsafe { CoTaskMemFree(Some(self.0.cast::<c_void>())) };
        }
    }
}

/// Reads one display name of `item`, freeing the Shell-allocated string.
fn read_display_name(item: &IShellItem, form: SIGDN) -> Option<String> {
    // SAFETY: `item` is a live Shell item. `GetDisplayName` allocates the returned
    // string with the COM task allocator, so it is wrapped in the freeing guard.
    let raw = unsafe { item.GetDisplayName(form) }.ok()?;
    CoTaskMemStrGuard(raw).to_text()
}

/// Reads one string property of `item`, freeing the Shell-allocated string.
fn read_string_property(item: &IShellItem2, key: &PROPERTYKEY) -> Option<String> {
    // SAFETY: `item` is a live Shell item and `key` is a well-formed `PROPERTYKEY`. The
    // returned string is COM task allocated and freed by the guard.
    let raw = unsafe { item.GetString(key) }.ok()?;
    CoTaskMemStrGuard(raw).to_text()
}

/// Reads every value the Recycle Bin listing needs from one child item.
fn read_shell_item(pidl: *const ITEMIDLIST, folder: &IShellFolder) -> Option<ShellItemFacts> {
    // SAFETY: `pidl` is a child identifier owned by the caller and `folder` is the bound
    // parent Shell folder, so the Shell can resolve the item; `IShellItem2` is requested.
    let item: IShellItem2 = unsafe { SHCreateItemWithParent(None, folder, pidl) }.ok()?;

    let name = read_display_name(&item, SIGDN_NORMALDISPLAY)?;
    let shell_path = read_display_name(&item, SIGDN_DESKTOPABSOLUTEPARSING).map(PathBuf::from)?;

    // SAFETY: `item` is a live Shell item and `PKEY_SIZE` is a well-formed key.
    let size = unsafe { item.GetUInt64(&PKEY_SIZE) }.unwrap_or(0);
    // SAFETY: `item` is a live Shell item and `PKEY_RECYCLE_DATE_DELETED` is a
    // well-formed key; an item without a deletion time yields an error, not a crash.
    let deleted_at = unsafe { item.GetFileTime(&PKEY_RECYCLE_DATE_DELETED) }
        .ok()
        .and_then(filetime_to_unix);
    let deleted_from = read_string_property(&item, &PKEY_RECYCLE_DELETED_FROM);

    let base: &IShellItem = &item;
    // SAFETY: `base` points at the same live Shell item and `SFGAO_FOLDER` is a valid
    // attribute mask; asking for the mask makes the Shell report whether it applies.
    let is_dir = unsafe { base.GetAttributes(SFGAO_FOLDER) }
        .is_ok_and(|attributes| attributes.contains(SFGAO_FOLDER));

    Some(ShellItemFacts {
        name,
        shell_path,
        deleted_from,
        size,
        deleted_at,
        is_dir,
    })
}

/// Enumerates the Windows Recycle Bin through the Shell namespace.
///
/// The Shell binds on the calling thread inside an STA apartment, so callers must
/// run this on a worker thread (Tokio `spawn_blocking`), never on the UI thread.
/// Items the Shell cannot name are skipped; a failing Shell folder is an error.
pub fn enumerate_recycle_bin() -> OnyxResult<Vec<RecycleBinItem>> {
    // COM must be initialized on this exact thread for the Shell interfaces below.
    let _com = ComGuard::init_sta()?;

    // SAFETY: COM is initialized on this thread and the returned `IShellItem` is released
    // by the interface's own refcount. The known-folder item of the Recycle Bin is the
    // entry point into the Shell namespace folder.
    let known_item: IShellItem =
        unsafe { SHGetKnownFolderItem(&FOLDERID_RecycleBinFolder, KF_FLAG_DEFAULT, None) }
            .map_err(|err| OnyxError::Platform {
                message: format!("Could not resolve the Recycle Bin Shell item: {err}"),
            })?;

    // SAFETY: `known_item` is a live Shell item; `BHID_SFObject` hands out the item's
    // `IShellFolder` handler, which the Shell reference-counts with the item.
    let folder: IShellFolder =
        unsafe { known_item.BindToHandler(None, &BHID_SFObject) }.map_err(|err| {
            OnyxError::Platform {
                message: format!("Could not bind the Recycle Bin Shell folder: {err}"),
            }
        })?;

    let mut enumerator: Option<IEnumIDList> = None;
    // SAFETY: `folder` is a live Shell folder, a null owner window is allowed and the
    // out-parameter points at writable storage. Both item kinds are requested, and
    // hidden items stay listed because the Explorer Recycle Bin shows them too.
    unsafe {
        folder.EnumObjects(
            HWND::default(),
            (SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0 | SHCONTF_INCLUDEHIDDEN.0) as u32,
            &raw mut enumerator,
        )
    }
    .ok()
    .map_err(|err| OnyxError::Platform {
        message: format!("Could not enumerate the Recycle Bin: {err}"),
    })?;

    let Some(enumerator) = enumerator else {
        return Ok(Vec::new());
    };

    let mut items = Vec::new();
    loop {
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        // SAFETY: the enumerator is live and `pidl` points at writable storage for one
        // child identifier; the Shell returns `S_FALSE` once the list is exhausted.
        let status = unsafe { enumerator.Next(std::slice::from_mut(&mut pidl), None) };
        if status.is_err() || pidl.is_null() {
            break;
        }
        let pidl = PidlGuard(pidl);
        if let Some(item) = read_shell_item(pidl.0, &folder).and_then(ShellItemFacts::into_item) {
            items.push(item);
        } else {
            tracing::debug!("Recycle Bin item without a readable name was skipped");
        }
    }

    Ok(items)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn facts(deleted_from: Option<&str>) -> ShellItemFacts {
        ShellItemFacts {
            name: "report.txt".to_string(),
            shell_path: PathBuf::from(r"::{645FF040-5081-101B-9F08-00AA002F954E}\$RABCDEF.txt"),
            deleted_from: deleted_from.map(str::to_string),
            size: 4096,
            deleted_at: Some(1_700_000_000),
            is_dir: false,
        }
    }

    #[test]
    fn deleted_from_folder_is_joined_with_the_item_name() {
        let item = facts(Some(r"C:\Users\me\Desktop")).into_item().unwrap();
        assert_eq!(
            item.original_path,
            Some(PathBuf::from(r"C:\Users\me\Desktop\report.txt"))
        );
        assert_eq!(item.name, "report.txt");
        assert_eq!(item.size, 4096);
        assert_eq!(item.deleted_at, Some(1_700_000_000));
        assert!(!item.is_dir);
    }

    #[test]
    fn a_full_original_path_is_not_extended_twice() {
        let item = facts(Some(r"C:\Users\me\Desktop\report.txt"))
            .into_item()
            .unwrap();
        assert_eq!(
            item.original_path,
            Some(PathBuf::from(r"C:\Users\me\Desktop\report.txt"))
        );
    }

    #[test]
    fn a_report_as_full_path_display_name_yields_the_file_name() {
        // Builds that report the full path as the display name still show the file
        // name in the list and keep the original location intact.
        let mut full_path_name = facts(Some(r"C:\Users\me\Desktop"));
        full_path_name.name = r"C:\Users\me\Desktop\report.txt".to_string();
        let item = full_path_name.into_item().unwrap();
        assert_eq!(item.name, "report.txt");
        assert_eq!(
            item.original_path,
            Some(PathBuf::from(r"C:\Users\me\Desktop\report.txt"))
        );
    }

    #[test]
    fn name_comparison_ignores_ascii_case() {
        let item = facts(Some(r"C:\Users\me\Desktop\REPORT.TXT"))
            .into_item()
            .unwrap();
        assert_eq!(
            item.original_path,
            Some(PathBuf::from(r"C:\Users\me\Desktop\REPORT.TXT"))
        );
    }

    #[test]
    fn missing_or_blank_original_location_stays_unknown() {
        assert_eq!(facts(None).into_item().unwrap().original_path, None);
        assert_eq!(facts(Some("   ")).into_item().unwrap().original_path, None);
    }

    #[test]
    fn items_without_a_name_or_shell_path_are_dropped() {
        let mut nameless = facts(Some(r"C:\Temp"));
        nameless.name = "  ".to_string();
        assert!(nameless.into_item().is_none());

        let mut pathless = facts(Some(r"C:\Temp"));
        pathless.shell_path = PathBuf::new();
        assert!(pathless.into_item().is_none());
    }

    #[test]
    fn filetime_zero_reports_no_deletion_time() {
        assert_eq!(filetime_to_unix(FILETIME::default()), None);
    }

    #[test]
    fn filetime_converts_to_unix_seconds() {
        // 2023-11-14T22:13:20Z in 100ns ticks since 1601-01-01.
        let ticks =
            (1_700_000_000 + FILETIME_UNIX_EPOCH_OFFSET_SECS) as u64 * FILETIME_TICKS_PER_SEC;
        let file_time = FILETIME {
            dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        assert_eq!(filetime_to_unix(file_time), Some(1_700_000_000));
    }

    #[test]
    fn enumerating_returns_named_items_only() {
        // The Recycle Bin may be empty or populated; every item that comes back must
        // carry the name the UI shows and a unique Shell path.
        let Ok(items) = enumerate_recycle_bin() else {
            // A machine without a usable Recycle Bin must not fail the test suite.
            return;
        };
        let mut seen = std::collections::HashSet::new();
        for item in &items {
            assert!(!item.name.trim().is_empty());
            assert!(!item.shell_path.as_os_str().is_empty());
            assert!(seen.insert(item.shell_path.clone()), "duplicate shell path");
        }
    }
}
