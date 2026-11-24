//! Native Windows Shell context menu for items in a common folder.

use crate::{ComGuard, own_window, recycle_paths, to_wide_null, wide_slice_to_string};
use onyx_core::{OnyxError, OnyxResult, normalize_display_path};
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CMF_EXPLORE, CMF_NORMAL, CMIC_MASK_PTINVOKE, CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX,
    DefSubclassProc, GCS_VERBW, IContextMenu, IContextMenu2, IContextMenu3, ILFindLastID,
    ILIsParent, IShellFolder, RemoveWindowSubclass, SEE_MASK_UNICODE, SHBindToParent,
    SHParseDisplayName, SetWindowSubclass,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DestroyMenu, DestroyWindow, EndMenu, GetCursorPos,
    GetForegroundWindow, HMENU, KillTimer, SW_SHOWNORMAL, SetForegroundWindow, SetTimer,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx, WM_DRAWITEM, WM_INITMENUPOPUP,
    WM_MEASUREITEM, WM_MENUCHAR, WM_NCDESTROY, WM_TIMER, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{Interface, PCSTR, PCWSTR, PSTR, w};

const FIRST_COMMAND: u32 = 1;
const LAST_COMMAND: u32 = 0x7fff;
const MENU_TIMER: usize = 1;
const MENU_TIMEOUT_MS: u32 = 120_000;

/// Displays a native Shell menu at the cursor for one or more items in the same folder.
///
/// Call from a blocking worker with no existing MTA initialization. The caller waits
/// for menu dismissal and for any selected Shell command to return.
pub fn show_context_menu(hwnd: isize, paths: &[PathBuf]) -> OnyxResult<()> {
    if paths.is_empty() {
        return Err(invalid_path("The shell menu requires at least one path"));
    }
    if paths.len() > u32::MAX as usize {
        return Err(invalid_path("Too many paths for one shell menu"));
    }
    let owner = own_window(hwnd)?;
    let mut cursor = POINT::default();
    // SAFETY: The cursor result is written into a live POINT.
    unsafe { GetCursorPos(&raw mut cursor) }.map_err(|err| platform("GetCursorPos", err))?;

    let resolved = resolve_paths(paths)?;
    let parent = resolved[0]
        .parent()
        .ok_or_else(|| invalid_path("The shell menu requires items with a parent folder"))?;

    let com = ComGuard::init_sta()?;
    if !com.owns_initialization() {
        return Err(OnyxError::Platform {
            message: "The shell menu must run on an STA worker thread".to_string(),
        });
    }

    let menu_owner = MenuOwner::new(owner)?;
    let (context, menu) = build_menu(&menu_owner, parent, &resolved)?;
    let selection = menu_owner.track(&context, &menu, cursor, owner)?;
    if selection == 0 {
        return Ok(());
    }
    if !(FIRST_COMMAND..=LAST_COMMAND).contains(&selection) {
        return Err(OnyxError::Platform {
            message: format!("The Shell returned an unexpected menu command: {selection}"),
        });
    }

    let offset = (selection - FIRST_COMMAND) as usize;
    if is_delete_command(&context, offset) {
        return recycle_paths(&resolved);
    }

    let directory = shell_path(parent);
    let invoke = CMINVOKECOMMANDINFOEX {
        cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: SEE_MASK_UNICODE | CMIC_MASK_PTINVOKE,
        hwnd: owner,
        lpVerb: PCSTR(offset as *const u8),
        lpVerbW: PCWSTR(offset as *const u16),
        lpDirectoryW: PCWSTR(directory.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ptInvoke: cursor,
        ..Default::default()
    };
    // SAFETY: The extended invocation struct has the required prefix and size.
    // Numeric verbs encode the command offset, and the directory buffer outlives the call.
    unsafe { context.InvokeCommand((&raw const invoke).cast::<CMINVOKECOMMANDINFO>()) }
        .map_err(|err| platform("IContextMenu::InvokeCommand", err))
}

fn build_menu(
    menu_owner: &MenuOwner,
    parent: &Path,
    resolved: &[PathBuf],
) -> OnyxResult<(IContextMenu, HMenuGuard)> {
    let parent_pidl = PidlGuard::parse(parent)?;
    let items = resolved
        .iter()
        .map(|path| {
            let item = PidlGuard::parse(path)?;
            // SAFETY: Both PIDLs are valid absolute Shell item lists.
            if !unsafe { ILIsParent(parent_pidl.0, item.0, true) }.as_bool() {
                return Err(invalid_path("Shell menu items must share a parent folder"));
            }
            Ok(item)
        })
        .collect::<OnyxResult<Vec<_>>>()?;

    // SAFETY: The selection is nonempty and the first PIDL is valid. No child output is needed.
    let folder: IShellFolder = unsafe { SHBindToParent(items[0].0, None) }
        .map_err(|err| platform("SHBindToParent", err))?;
    let child_pidls: Vec<_> = items
        .iter()
        .map(|item| {
            // SAFETY: Every item is an immediate child of the parent verified above.
            // The last ID belongs to item and borrows its allocation.
            unsafe { ILFindLastID(item.0) }.cast_const()
        })
        .collect();

    // SAFETY: Each child PIDL belongs to the same parent and remains live in items.
    let context: IContextMenu =
        unsafe { folder.GetUIObjectOf(menu_owner.hwnd, &child_pidls, None) }
            .map_err(|err| platform("IShellFolder::GetUIObjectOf", err))?;
    // SAFETY: The newly created menu will be destroyed by HMenuGuard.
    let menu =
        HMenuGuard(unsafe { CreatePopupMenu() }.map_err(|err| platform("CreatePopupMenu", err))?);
    // SAFETY: Both objects are live on this STA, and the specified command range is valid.
    unsafe {
        context.QueryContextMenu(
            menu.0,
            0,
            FIRST_COMMAND,
            LAST_COMMAND,
            CMF_NORMAL | CMF_EXPLORE,
        )
    }
    .ok()
    .map_err(|err| platform("IContextMenu::QueryContextMenu", err))?;

    Ok((context, menu))
}

fn is_delete_command(context: &IContextMenu, offset: usize) -> bool {
    let mut verb = [0u16; 256];
    // SAFETY: For GCS_VERBW, GetCommandString writes UTF-16 into the pointer
    // declared as PSTR. The capacity is expressed in UTF-16 units.
    let result = unsafe {
        context.GetCommandString(
            offset,
            GCS_VERBW,
            None,
            PSTR(verb.as_mut_ptr().cast()),
            verb.len() as u32,
        )
    };
    result.is_ok() && wide_slice_to_string(&verb).eq_ignore_ascii_case("delete")
}

fn resolve_paths(paths: &[PathBuf]) -> OnyxResult<Vec<PathBuf>> {
    paths
        .iter()
        .map(|path| {
            if path.as_os_str().is_empty() || path.as_os_str().encode_wide().any(|unit| unit == 0) {
                return Err(invalid_path(
                    "The Shell menu received an empty or malformed path",
                ));
            }
            let path_error = |error: std::io::Error| match error.kind() {
                ErrorKind::NotFound => OnyxError::NotFound { path: path.clone() },
                ErrorKind::InvalidInput | ErrorKind::InvalidFilename => {
                    invalid_path(format!("Invalid shell item '{}': {error}", path.display()))
                }
                _ => error.into(),
            };
            let absolute = std::path::absolute(path).map_err(path_error)?;
            std::fs::symlink_metadata(&absolute).map_err(path_error)?;
            Ok(absolute)
        })
        .collect()
}

fn shell_path(path: &Path) -> Vec<u16> {
    // Shell parsing uses ordinary filesystem names. Preserve the original UTF-16 for
    // ordinary paths, including filenames with unpaired surrogates.
    if path.to_str().is_some_and(|path| path.starts_with(r"\\?\")) {
        to_wide_null(OsStr::new(&normalize_display_path(path)))
    } else {
        to_wide_null(path.as_os_str())
    }
}

struct PidlGuard(*mut ITEMIDLIST);

impl PidlGuard {
    fn parse(path: &Path) -> OnyxResult<Self> {
        let wide = shell_path(path);
        let mut item = Self(null_mut());
        // SAFETY: wide is null-terminated and item.0 points to writable PIDL storage.
        unsafe { SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &raw mut item.0, 0, None) }
            .map_err(|err| {
                if std::fs::symlink_metadata(path)
                    .is_err_and(|err| err.kind() == ErrorKind::NotFound)
                {
                    OnyxError::NotFound {
                        path: path.to_path_buf(),
                    }
                } else {
                    invalid_path(format!(
                        "The Shell could not parse '{}': {err}",
                        path.display()
                    ))
                }
            })?;
        if item.0.is_null() {
            return Err(invalid_path("The Shell did not return an item identifier"));
        }
        Ok(item)
    }
}

impl Drop for PidlGuard {
    fn drop(&mut self) {
        // SAFETY: The Shell allocated this PIDL with the COM allocator. Null is allowed.
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

struct HMenuGuard(HMENU);

impl Drop for HMenuGuard {
    fn drop(&mut self) {
        // SAFETY: This guard owns the unattached popup menu and its submenus.
        let _ = unsafe { DestroyMenu(self.0) };
    }
}

struct MenuOwner {
    hwnd: HWND,
}

impl MenuOwner {
    fn new(owner: HWND) -> OnyxResult<Self> {
        // SAFETY: STATIC is a system-defined class. WS_POPUP uses owner as an owner
        // HWND. The window belongs to this worker, so Shell menu messages stay on its STA.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("STATIC"),
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                Some(owner),
                None,
                None,
                None,
            )
        }
        .map_err(|err| platform("CreateWindowExW", err))?;
        Ok(Self { hwnd })
    }

    fn track(
        &self,
        context: &IContextMenu,
        menu: &HMenuGuard,
        cursor: POINT,
        owner: HWND,
    ) -> OnyxResult<u32> {
        let messages = MenuMessages {
            version_3: context.cast().ok(),
            version_2: context.cast().ok(),
        };
        // SAFETY: The subclass is installed on this thread's temporary window and
        // SubclassGuard removes it before the borrowed message handler leaves scope.
        if !unsafe {
            SetWindowSubclass(self.hwnd, Some(menu_proc), 0, &raw const messages as usize)
        }
        .as_bool()
        {
            return Err(OnyxError::Platform {
                message: "Could not install the Shell menu message handler".to_string(),
            });
        }
        let _subclass = SubclassGuard(self.hwnd);
        // SAFETY: The timer and modal menu are owned by this worker window. The timer
        // is cancelled immediately after TrackPopupMenuEx returns.
        let selection = unsafe {
            if GetForegroundWindow() == owner {
                let _ = SetForegroundWindow(self.hwnd);
            }
            if SetTimer(Some(self.hwnd), MENU_TIMER, MENU_TIMEOUT_MS, None) == 0 {
                return Err(platform("SetTimer", windows::core::Error::from_thread()));
            }
            let selected = TrackPopupMenuEx(
                menu.0,
                (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
                cursor.x,
                cursor.y,
                self.hwnd,
                None,
            );
            let _ = KillTimer(Some(self.hwnd), MENU_TIMER);
            selected.0 as u32
        };
        Ok(selection)
    }
}

impl Drop for MenuOwner {
    fn drop(&mut self) {
        // SAFETY: Destroy the owned temporary window on its creating thread.
        let _ = unsafe { DestroyWindow(self.hwnd) };
    }
}

struct SubclassGuard(HWND);

impl Drop for SubclassGuard {
    fn drop(&mut self) {
        // SAFETY: Removes the exact subclass registered on this thread's menu owner.
        let _ = unsafe { RemoveWindowSubclass(self.0, Some(menu_proc), 0) };
    }
}

struct MenuMessages {
    version_3: Option<IContextMenu3>,
    version_2: Option<IContextMenu2>,
}

unsafe extern "system" fn menu_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    reference: usize,
) -> LRESULT {
    if msg == WM_TIMER && wparam.0 == MENU_TIMER {
        // SAFETY: The timer belongs to the currently tracking menu's window and thread.
        let _ = unsafe { EndMenu() };
        return LRESULT(0);
    }
    if msg == WM_NCDESTROY {
        // SAFETY: This is the subclass installed on the window being destroyed.
        let _ = unsafe { RemoveWindowSubclass(hwnd, Some(menu_proc), 0) };
    }
    if matches!(
        msg,
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
    ) {
        // SAFETY: SubclassGuard ties reference to a live MenuMessages for callbacks.
        let messages = unsafe { &*(reference as *const MenuMessages) };
        let mut result = LRESULT(0);
        if let Some(context) = &messages.version_3 {
            // SAFETY: This callback forwards unmodified menu messages on the same STA
            // and supplies writable storage for the optional message result.
            if unsafe { context.HandleMenuMsg2(msg, wparam, lparam, Some(&raw mut result)) }.is_ok()
            {
                return result;
            }
        } else if msg != WM_MENUCHAR
            && let Some(context) = &messages.version_2
        {
            // SAFETY: Original menu message parameters are valid for the synchronous call.
            if unsafe { context.HandleMenuMsg(msg, wparam, lparam) }.is_ok() {
                return LRESULT(isize::from(msg != WM_INITMENUPOPUP));
            }
        }
    }
    // SAFETY: Forward the original arguments through the subclass chain.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

fn invalid_path(message: impl Into<String>) -> OnyxError {
    OnyxError::InvalidPath {
        message: message.into(),
    }
}

fn platform(api: &str, error: windows::core::Error) -> OnyxError {
    OnyxError::Platform {
        message: format!("{api} failed: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{GetMenuItemCount, GetMenuItemID};

    #[test]
    fn rejects_empty_selection() {
        assert!(matches!(
            show_context_menu(0, &[]),
            Err(OnyxError::InvalidPath { .. })
        ));
    }

    #[test]
    fn rejects_invalid_window() {
        assert!(matches!(
            show_context_menu(0, &[PathBuf::from("item")]),
            Err(OnyxError::Platform { .. })
        ));
    }

    #[test]
    fn rejects_missing_path() -> OnyxResult<()> {
        let temp = tempfile::tempdir()?;
        let missing = temp.path().join("missing.txt");
        let window = MenuOwner::new(HWND::default())?;
        assert!(matches!(
            show_context_menu(window.hwnd.0 as isize, std::slice::from_ref(&missing)),
            Err(OnyxError::NotFound { path }) if path == missing
        ));
        Ok(())
    }

    #[test]
    fn builds_native_menu_for_multiple_files() -> OnyxResult<()> {
        let temp = tempfile::tempdir()?;
        let paths = [temp.path().join("one.txt"), temp.path().join("two.txt")];
        for path in &paths {
            std::fs::File::create(path)?;
        }
        let _com = ComGuard::init_sta()?;
        let owner = MenuOwner::new(HWND::default())?;
        let (context, menu) = build_menu(&owner, temp.path(), &paths)?;
        // SAFETY: The test queries IDs on a live native menu without invoking commands.
        let has_delete = unsafe {
            (0..GetMenuItemCount(Some(menu.0))).any(|index| {
                let id = GetMenuItemID(menu.0, index);
                (FIRST_COMMAND..=LAST_COMMAND).contains(&id)
                    && is_delete_command(&context, (id - FIRST_COMMAND) as usize)
            })
        };
        assert!(has_delete, "the native Delete verb must be recognized");
        Ok(())
    }
}
