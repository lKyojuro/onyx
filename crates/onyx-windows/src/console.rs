use windows::Win32::Foundation::HANDLE;
use windows::core::BOOL;

const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
const STD_OUTPUT_HANDLE: u32 = -11_i32 as u32;
const STD_ERROR_HANDLE: u32 = -12_i32 as u32;

// These kernel32 exports are available without enabling the Win32_System_Console feature.
#[link(name = "kernel32")]
unsafe extern "system" {
    fn AttachConsole(process_id: u32) -> BOOL;
    fn GetStdHandle(std_handle: u32) -> HANDLE;
    fn SetStdHandle(std_handle: u32, handle: HANDLE) -> BOOL;
}

/// Attempts to attach to the parent's console without creating a new one.
///
/// Call at startup, before starting threads or writing to stdout or stderr.
pub fn attach_parent_console() -> bool {
    // SAFETY: All selectors are documented Win32 constants. The handles are borrowed
    // from this process and are restored, if valid, to preserve inherited redirection.
    unsafe {
        let stdout = GetStdHandle(STD_OUTPUT_HANDLE);
        let stderr = GetStdHandle(STD_ERROR_HANDLE);
        if !AttachConsole(ATTACH_PARENT_PROCESS).as_bool() {
            return false;
        }
        for (selector, handle) in [(STD_OUTPUT_HANDLE, stdout), (STD_ERROR_HANDLE, stderr)] {
            if !handle.is_invalid() {
                let _ = SetStdHandle(selector, handle);
            }
        }
        true
    }
}
