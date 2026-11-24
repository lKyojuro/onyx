//! Windows Shell actions: file opening, Open With, Properties dialog, Terminal launch,
//! and safe Recycle Bin deletion via COM `IFileOperation`.

use crate::com::ComGuard;
use crate::to_wide_null;
use onyx_core::{OnyxError, OnyxResult, normalize_display_path};
use std::ffi::OsStr;
use std::mem::size_of;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::Threading::CREATE_NEW_CONSOLE;
use windows::Win32::UI::Shell::{
    FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOFX_RECYCLEONDELETE,
    FileOperation, IFileOperation, IShellItem, SEE_MASK_INVOKEIDLIST, SHCreateItemFromParsingName,
    SHELLEXECUTEINFOW, ShellExecuteExW, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::PCWSTR;

/// Opens a file or folder with its registered default Windows shell handler (`ShellExecuteW`).
pub fn open_file(path: &Path) -> OnyxResult<()> {
    if !path.exists() {
        return Err(OnyxError::NotFound {
            path: path.to_path_buf(),
        });
    }

    let _com = ComGuard::init_sta().ok();
    let verb_w = to_wide_null(OsStr::new("open"));
    let path_display = normalize_display_path(path);
    let path_w = to_wide_null(OsStr::new(&path_display));

    // SAFETY: Both `verb_w` and `path_w` are valid null-terminated UTF-16 strings.
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb_w.as_ptr()),
            PCWSTR(path_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };

    if (result.0 as isize) > 32 {
        Ok(())
    } else {
        Err(OnyxError::Platform {
            message: format!(
                "ShellExecuteW failed to open '{}' (code {})",
                path.display(),
                result.0 as isize
            ),
        })
    }
}

/// Displays the Windows "Open With" dialog for `path`.
pub fn open_with_dialog(path: &Path) -> OnyxResult<()> {
    if !path.exists() {
        return Err(OnyxError::NotFound {
            path: path.to_path_buf(),
        });
    }

    let path_display = normalize_display_path(path);
    Command::new("rundll32.exe")
        .arg("shell32.dll,OpenAs_RunDLL")
        .arg(&path_display)
        .spawn()
        .map(|_| ())
        .map_err(|err| OnyxError::Platform {
            message: format!(
                "Failed to launch Open With dialog for '{}': {err}",
                path.display()
            ),
        })
}

/// Displays the native Windows Shell Properties dialog for `path`.
pub fn show_properties_dialog(path: &Path) -> OnyxResult<()> {
    if !path.exists() {
        return Err(OnyxError::NotFound {
            path: path.to_path_buf(),
        });
    }

    let _com = ComGuard::init_sta()?;
    let verb_w = to_wide_null(OsStr::new("properties"));
    let path_display = normalize_display_path(path);
    let path_w = to_wide_null(OsStr::new(&path_display));

    let mut sei = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_INVOKEIDLIST,
        lpVerb: PCWSTR(verb_w.as_ptr()),
        lpFile: PCWSTR(path_w.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    // SAFETY: `sei.cbSize` matches `SHELLEXECUTEINFOW` and both UTF-16 buffers outlive the call.
    unsafe { ShellExecuteExW(&raw mut sei) }.map_err(|err| OnyxError::Platform {
        message: format!(
            "ShellExecuteExW(properties) failed for '{}': {err}",
            path.display()
        ),
    })
}

/// Launches Windows Terminal (`wt.exe -d <dir>`), falling back to `powershell.exe` in `dir`
/// if Windows Terminal is not installed.
pub fn launch_terminal(dir: &Path) -> OnyxResult<()> {
    launch_terminal_with_profile(dir, TerminalProfile::WindowsTerminal)
}

/// Terminal application to open for a file or directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalProfile {
    WindowsTerminal,
    PowerShell,
    Cmd,
}

/// Opens the selected terminal profile in `dir` or in the containing directory of a file.
/// The Windows Terminal profile falls back to Windows PowerShell if its launcher fails.
pub fn launch_terminal_with_profile(dir: &Path, profile: TerminalProfile) -> OnyxResult<()> {
    if !dir.exists() {
        return Err(OnyxError::NotFound {
            path: dir.to_path_buf(),
        });
    }

    let absolute = std::path::absolute(dir)?;
    let target_dir = if absolute.is_dir() {
        absolute.as_path()
    } else {
        absolute.parent().unwrap_or(&absolute)
    };

    let launch_console = |program| {
        Command::new(program)
            .current_dir(target_dir)
            .creation_flags(CREATE_NEW_CONSOLE.0)
            .spawn()
            .map(|_| ())
            .map_err(|error| OnyxError::Platform {
                message: format!(
                    "Failed to launch {program} in '{}': {error}",
                    target_dir.display()
                ),
            })
    };
    match profile {
        TerminalProfile::PowerShell => launch_console("powershell.exe"),
        TerminalProfile::Cmd => launch_console("cmd.exe"),
        TerminalProfile::WindowsTerminal => {
            let terminal = Command::new("wt.exe")
                .arg("-d")
                .arg(normalize_display_path(target_dir))
                .spawn();
            if let Err(error) = terminal {
                tracing::debug!(%error, "wt.exe unavailable; falling back to powershell.exe");
                return launch_console("powershell.exe").map_err(|fallback| OnyxError::Platform {
                    message: format!("Could not launch wt.exe: {error}. {fallback}"),
                });
            }
            Ok(())
        }
    }
}

/// Moves `paths` safely to the Windows Recycle Bin using COM `IFileOperation` on a dedicated
/// STA thread.
///
/// This function NEVER performs permanent deletion; it configures `IFileOperation` with
/// `FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI | FOFX_RECYCLEONDELETE`.
pub fn recycle_paths(paths: &[PathBuf]) -> OnyxResult<()> {
    if paths.is_empty() {
        return Ok(());
    }

    let mut resolved_paths = Vec::with_capacity(paths.len());
    for path in paths {
        if std::fs::symlink_metadata(path).is_err() {
            return Err(OnyxError::NotFound { path: path.clone() });
        }
        let abs = if path.is_absolute() {
            path.clone()
        } else {
            std::env::current_dir()?.join(path)
        };
        resolved_paths.push(normalize_display_path(&abs));
    }

    // Run on a dedicated OS thread to guarantee a clean STA apartment (`IFileOperation` requires STA).
    let handle = std::thread::Builder::new()
        .name("onyx-recycle-sta".to_string())
        .spawn(move || recycle_paths_sta(&resolved_paths))
        .map_err(|err| OnyxError::Platform {
            message: format!("Failed to spawn STA thread for Recycle Bin operation: {err}"),
        })?;

    handle.join().map_err(|_| OnyxError::Platform {
        message: "Recycle Bin STA worker thread panicked".to_string(),
    })?
}

fn recycle_paths_sta(paths: &[String]) -> OnyxResult<()> {
    let _com = ComGuard::init_sta()?;

    // SAFETY: COM is initialized in STA mode on this dedicated thread; `FileOperation` is a
    // standard Windows Shell CLSID implementing `IFileOperation`.
    let file_op: IFileOperation = unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_ALL) }
        .map_err(|err| OnyxError::Platform {
            message: format!("Failed to create IFileOperation instance: {err}"),
        })?;

    let flags =
        FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI | FOFX_RECYCLEONDELETE;

    // SAFETY: `file_op` is a valid COM interface pointer on this STA thread.
    unsafe { file_op.SetOperationFlags(flags) }.map_err(|err| OnyxError::Platform {
        message: format!("IFileOperation::SetOperationFlags failed: {err}"),
    })?;

    for path_str in paths {
        let wide = to_wide_null(OsStr::new(path_str));
        // SAFETY: `wide` is a valid null-terminated UTF-16 path and `IShellItem` is requested.
        let item: IShellItem = unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None) }
            .map_err(|err| OnyxError::Platform {
                message: format!("SHCreateItemFromParsingName failed for '{path_str}': {err}"),
            })?;

        // SAFETY: `file_op` and `item` are valid COM objects on the same STA thread.
        unsafe { file_op.DeleteItem(&item, None) }.map_err(|err| OnyxError::Platform {
            message: format!("IFileOperation::DeleteItem failed for '{path_str}': {err}"),
        })?;
    }

    // SAFETY: All queued items are valid and flags explicitly enforce Recycle Bin behavior.
    unsafe { file_op.PerformOperations() }.map_err(|err| OnyxError::Platform {
        message: format!("IFileOperation::PerformOperations failed: {err}"),
    })?;

    // SAFETY: Querying abort status after `PerformOperations` is valid on `file_op`.
    let aborted = unsafe { file_op.GetAnyOperationsAborted() }
        .map(|b| b.as_bool())
        .unwrap_or(false);

    if aborted {
        return Err(OnyxError::OperationFailed {
            message: "Recycle Bin operation was aborted before completing".to_string(),
        });
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn missing_paths_return_not_found() {
        let missing = PathBuf::from(r"C:\__onyx_nonexistent_path_404__\file.txt");
        assert!(matches!(
            open_file(&missing),
            Err(OnyxError::NotFound { .. })
        ));
        assert!(matches!(
            open_with_dialog(&missing),
            Err(OnyxError::NotFound { .. })
        ));
        assert!(matches!(
            show_properties_dialog(&missing),
            Err(OnyxError::NotFound { .. })
        ));
        assert!(matches!(
            launch_terminal(&missing),
            Err(OnyxError::NotFound { .. })
        ));
        assert!(matches!(
            recycle_paths(&[missing]),
            Err(OnyxError::NotFound { .. })
        ));
    }

    #[test]
    fn recycle_empty_slice_is_noop() {
        assert!(recycle_paths(&[]).is_ok());
    }
}
