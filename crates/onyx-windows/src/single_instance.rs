//! Single-instance IPC over a per-user Win32 named pipe.
//!
//! A second `onyx.exe` launch detects the running instance through
//! `\\.\pipe\onyx-explorer-ipc-v1-<user>` and forwards its command line instead of
//! building a whole application process. The fast path is a single `CreateFileW`
//! against the pipe: when nothing listens the kernel answers
//! `ERROR_FILE_NOT_FOUND` in microseconds, so a cold launch pays essentially
//! nothing for the check.
//!
//! The listener runs on its own thread and hands each parsed [`IpcCommand`] to a
//! callback; the caller moves it onto the GPUI loop. The client reads the
//! server's process ID back and calls `AllowSetForegroundWindow`, which lets the
//! warm instance raise its window even though the foreground lock belongs to the
//! freshly started (and about to exit) process.

use crate::to_wide_null;
use onyx_core::{OnyxError, OnyxResult};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ,
    GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FlushFileBuffers, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    WaitNamedPipeW,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, IsIconic, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow,
};
use windows::core::PCWSTR;

/// Prefix of the per-user pipe name; the sanitized user name is appended.
const PIPE_PREFIX: &str = r"\\.\pipe\onyx-explorer-ipc-v1-";

/// Command prefix for "open this path in the running instance".
const OPEN_PREFIX: &str = "OPEN:";

/// Command that only asks the running instance to come to the foreground.
const SHOW_COMMAND: &str = "SHOW";

/// Longest request the server reads off a client connection.
const MAX_REQUEST_BYTES: usize = 4096;

/// How long a client waits for a busy pipe instance to become available.
const PIPE_WAIT_MS: u32 = 250;

/// In/out buffer size of every pipe instance.
const PIPE_BUFFER_BYTES: u32 = 4096;

/// Longest user name fragment kept in the pipe name.
const MAX_USER_FRAGMENT: usize = 64;

/// Number of connect attempts `SingleInstanceServer::drop` makes to unblock
/// `ConnectNamedPipe` on the listener thread.
const SHUTDOWN_WAKE_ATTEMPTS: usize = 64;

/// A command forwarded from a secondary launch to the running instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcCommand {
    /// Show the window and navigate to the path when one was given.
    OpenPath(Option<PathBuf>),
    /// Show the window without navigating.
    ShowWindow,
}

impl IpcCommand {
    /// Encodes the command as the single UTF-8 line sent over the pipe.
    ///
    /// `OpenPath(None)` and `ShowWindow` mean the same thing to the server, so
    /// both travel as `SHOW`.
    #[must_use]
    pub fn to_wire_line(&self) -> String {
        match self {
            Self::OpenPath(Some(path)) => format!("{OPEN_PREFIX}{}\n", path.display()),
            Self::OpenPath(None) | Self::ShowWindow => format!("{SHOW_COMMAND}\n"),
        }
    }

    /// Parses a request line received on the server side.
    ///
    /// An `OPEN:` without a path degrades to `OpenPath(None)`, which the server
    /// treats exactly like `ShowWindow`.
    #[must_use]
    pub fn from_wire_line(line: &str) -> Option<Self> {
        let trimmed = line.trim_end_matches(['\r', '\n', '\0']).trim();
        if trimmed.eq_ignore_ascii_case(SHOW_COMMAND) || trimmed.is_empty() {
            return Some(Self::ShowWindow);
        }
        let rest = trimmed.get(..OPEN_PREFIX.len())?;
        if !rest.eq_ignore_ascii_case(OPEN_PREFIX) {
            return None;
        }
        let path = &trimmed[OPEN_PREFIX.len()..];
        if path.is_empty() {
            return Some(Self::OpenPath(None));
        }
        Some(Self::OpenPath(Some(PathBuf::from(path))))
    }
}

/// RAII guard closing an owned Win32 handle on drop.
#[derive(Debug)]
struct HandleGuard(HANDLE);

impl HandleGuard {
    /// Takes ownership of `handle`, rejecting the sentinel values Win32 uses for
    /// "no handle".
    fn new(handle: HANDLE) -> Option<Self> {
        (!handle.is_invalid() && handle != INVALID_HANDLE_VALUE).then_some(Self(handle))
    }

    /// Releases the handle as an `isize` so it can cross a thread boundary; the
    /// receiving thread must wrap it back in a `HandleGuard`.
    fn into_raw(self) -> isize {
        let raw = self.0.0 as isize;
        std::mem::forget(self);
        raw
    }
}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        // SAFETY: This guard owns the handle and closes it exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Listener of the single-instance pipe.
///
/// Dropping it stops the listener thread, wakes it out of `ConnectNamedPipe`
/// with one throwaway connection, and joins it, so no pipe or thread outlives
/// the explorer.
#[derive(Debug)]
pub struct SingleInstanceServer {
    pipe_name: String,
    stop: Arc<AtomicBool>,
    listener: Option<JoinHandle<()>>,
}

impl SingleInstanceServer {
    /// Name of the pipe this server listens on.
    #[must_use]
    pub fn pipe_name(&self) -> &str {
        &self.pipe_name
    }
}

impl Drop for SingleInstanceServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let Some(listener) = self.listener.take() else {
            return;
        };
        // The listener blocks in `ConnectNamedPipe`; a connection is the only way
        // to release it. Retry until the thread observes the stop flag, because a
        // connection can race with the listener recreating its instance.
        for _ in 0..SHUTDOWN_WAKE_ATTEMPTS {
            if listener.is_finished() {
                break;
            }
            let _ = try_send_to_pipe(&self.pipe_name, None);
            if listener.is_finished() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let _ = listener.join();
    }
}

/// Name of the pipe shared by every Onyx instance of the current Windows user.
#[must_use]
pub fn default_ipc_pipe_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    format!("{PIPE_PREFIX}{}", sanitize_user_fragment(&user))
}

/// Keeps only characters that are safe inside a pipe name, falling back to
/// `default` for names that sanitize to nothing.
fn sanitize_user_fragment(user: &str) -> String {
    let sanitized: String = user
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(MAX_USER_FRAGMENT)
        .collect();
    if sanitized.is_empty() {
        "default".to_string()
    } else {
        sanitized
    }
}

/// Sends `path` (or a plain show request) to an already running Onyx.
///
/// Returns `false` when no instance listens on the default pipe, which is the
/// signal for the caller to continue with a normal cold start.
#[must_use]
pub fn try_send_to_existing_instance(path: Option<&Path>) -> bool {
    try_send_to_pipe(&default_ipc_pipe_name(), path)
}

/// Sends `path` (or a plain show request) to the instance listening on
/// `pipe_name`.
///
/// A missing pipe answers in well under a millisecond because the failure is
/// reported by the first `CreateFileW`; only a busy pipe instance waits, and only
/// for [`PIPE_WAIT_MS`].
#[must_use]
pub fn try_send_to_pipe(pipe_name: &str, path: Option<&Path>) -> bool {
    let pipe_name_w = to_wide_null(OsStr::new(pipe_name));
    let Some(pipe) = connect_client(&pipe_name_w) else {
        return false;
    };
    let command = match path {
        Some(path) => IpcCommand::OpenPath(Some(path.to_path_buf())),
        None => IpcCommand::ShowWindow,
    };
    let payload = command.to_wire_line();
    // SAFETY: `pipe` is a connected client handle we own and `payload` outlives
    // the call.
    if unsafe { WriteFile(pipe.0, Some(payload.as_bytes()), None, None) }.is_err() {
        return false;
    }

    let mut reply = [0u8; 4];
    let mut read = 0u32;
    // SAFETY: reading up to four bytes into a live stack buffer; the server
    // writes exactly the four-byte length of its process ID.
    if unsafe { ReadFile(pipe.0, Some(&mut reply), Some(&raw mut read), None) }.is_ok() && read == 4
    {
        let server_pid = u32::from_le_bytes(reply);
        // SAFETY: the caller is the process Windows picked as foreground owner,
        // so it may hand that right to the server before exiting.
        let _ = unsafe { AllowSetForegroundWindow(server_pid) };
    }
    // The command reached the pipe; a missed reply only costs the focus handoff.
    true
}

/// Opens the client end of `pipe_name_w`, waiting once for a busy instance.
fn connect_client(pipe_name_w: &[u16]) -> Option<HandleGuard> {
    let mut waited = false;
    loop {
        // SAFETY: `pipe_name_w` is a null-terminated UTF-16 pipe name that
        // outlives the call; the returned handle is immediately owned by a guard.
        let opened = unsafe {
            CreateFileW(
                PCWSTR(pipe_name_w.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        };
        let err = match opened {
            Ok(handle) => return HandleGuard::new(handle),
            Err(err) => err,
        };
        let code = WIN32_ERROR::from_error(&err);
        if code == Some(ERROR_PIPE_BUSY) && !waited {
            waited = true;
            // SAFETY: waiting on the same pipe name for a free instance.
            if unsafe { WaitNamedPipeW(PCWSTR(pipe_name_w.as_ptr()), PIPE_WAIT_MS) }.as_bool() {
                continue;
            }
            return None;
        }
        // `ERROR_FILE_NOT_FOUND` means no instance exists: cold start, no cost.
        if code != Some(ERROR_FILE_NOT_FOUND) && code != Some(ERROR_PIPE_BUSY) {
            tracing::debug!(pipe_error = ?code, "single-instance pipe unavailable");
        }
        return None;
    }
}

/// Starts the single-instance listener on the default per-user pipe.
pub fn start_single_instance_server(
    on_command: impl Fn(IpcCommand) + Send + Sync + 'static,
) -> OnyxResult<SingleInstanceServer> {
    start_single_instance_server_at(&default_ipc_pipe_name(), on_command)
}

/// Starts the single-instance listener on `pipe_name`.
///
/// The first pipe instance is created on the calling thread so a name collision
/// fails here instead of on a worker thread: the instance carries
/// `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a second server on the same name is
/// rejected instead of silently stealing the connections of the running app.
pub fn start_single_instance_server_at(
    pipe_name: &str,
    on_command: impl Fn(IpcCommand) + Send + Sync + 'static,
) -> OnyxResult<SingleInstanceServer> {
    let pipe_name_w = to_wide_null(OsStr::new(pipe_name));
    let first = create_pipe_instance(&pipe_name_w, true).ok_or_else(|| OnyxError::Platform {
        message: format!("could not create the single-instance pipe {pipe_name}"),
    })?;

    let stop = Arc::new(AtomicBool::new(false));
    let listener_stop = Arc::clone(&stop);
    let callback: Arc<dyn Fn(IpcCommand) + Send + Sync> = Arc::new(on_command);
    let first_raw = first.into_raw();
    let listener = thread::Builder::new()
        .name("onyx-single-instance".to_string())
        .spawn(move || listen(first_raw, pipe_name_w, callback, listener_stop))
        .map_err(|error| OnyxError::Platform {
            message: format!("could not start the single-instance listener: {error}"),
        })?;

    Ok(SingleInstanceServer {
        pipe_name: pipe_name.to_string(),
        stop,
        listener: Some(listener),
    })
}

/// Creates one pipe instance available to a single client at a time.
///
/// `first` marks the instance that claims the pipe name for this process; later
/// instances of the same server omit the flag so clients waiting in
/// `WaitNamedPipeW` can connect to them.
fn create_pipe_instance(pipe_name_w: &[u16], first: bool) -> Option<HandleGuard> {
    let open_mode = if first {
        PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE
    } else {
        PIPE_ACCESS_DUPLEX
    };
    // SAFETY: `pipe_name_w` is a null-terminated UTF-16 pipe name that outlives
    // the call; `None` requests the default security descriptor of the process.
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(pipe_name_w.as_ptr()),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUFFER_BYTES,
            PIPE_BUFFER_BYTES,
            0,
            None,
        )
    };
    HandleGuard::new(handle)
}

/// Listener loop: connect one client, serve its request, repeat until stopped.
fn listen(
    first_raw: isize,
    pipe_name_w: Vec<u16>,
    on_command: Arc<dyn Fn(IpcCommand) + Send + Sync>,
    stop: Arc<AtomicBool>,
) {
    let mut next = Some(HandleGuard(HANDLE(first_raw as _)));
    loop {
        let pipe = match next.take() {
            Some(pipe) => pipe,
            None => {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                match create_pipe_instance(&pipe_name_w, false) {
                    Some(pipe) => pipe,
                    None => {
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                }
            }
        };

        // SAFETY: connecting the instance we own; no overlapped I/O is used, so
        // the call returns once a client connects or the instance is closed.
        let connected = match unsafe { ConnectNamedPipe(pipe.0, None) } {
            Ok(()) => true,
            // A client can connect between `CreateNamedPipeW` and this call.
            Err(err) => WIN32_ERROR::from_error(&err) == Some(ERROR_PIPE_CONNECTED),
        };
        let stopping = stop.load(Ordering::SeqCst);
        let command = if connected {
            read_request(pipe.0)
        } else {
            None
        };
        if connected && !stopping {
            write_server_pid(pipe.0);
        }
        // SAFETY: disconnecting the instance we own before it is reused.
        let _ = unsafe { DisconnectNamedPipe(pipe.0) };
        drop(pipe);

        if stopping {
            break;
        }
        match command {
            Some(command) => on_command(command),
            None if !connected => thread::sleep(Duration::from_millis(10)),
            None => {}
        }
    }
}

/// Reads one request line off a connected pipe instance.
fn read_request(pipe: HANDLE) -> Option<IpcCommand> {
    let mut buffer = [0u8; MAX_REQUEST_BYTES];
    let mut read = 0u32;
    // SAFETY: reading into a live stack buffer bounded by `MAX_REQUEST_BYTES`.
    if unsafe { ReadFile(pipe, Some(&mut buffer), Some(&raw mut read), None) }.is_err() {
        return None;
    }
    let len = (read as usize).min(buffer.len());
    let text = String::from_utf8_lossy(&buffer[..len]);
    IpcCommand::from_wire_line(&text)
}

/// Writes the server process ID as the four-byte reply the client waits for.
fn write_server_pid(pipe: HANDLE) -> bool {
    // SAFETY: `GetCurrentProcessId` has no preconditions.
    let bytes = unsafe { GetCurrentProcessId() }.to_le_bytes();
    // SAFETY: writing four bytes to the connected instance we own.
    let written = unsafe { WriteFile(pipe, Some(&bytes), None, None) }.is_ok();
    // SAFETY: flushing the same instance so the client's read completes.
    written && unsafe { FlushFileBuffers(pipe) }.is_ok()
}

/// Brings an Onyx window to the foreground, restoring it when minimized.
pub fn focus_window(hwnd: isize) -> OnyxResult<()> {
    let window = crate::own_window(hwnd)?;
    // SAFETY: `window` was validated to be a live window of this process above.
    unsafe {
        if IsIconic(window).as_bool() {
            let _ = ShowWindow(window, SW_RESTORE);
        } else {
            let _ = ShowWindow(window, SW_SHOW);
        }
        let _ = SetForegroundWindow(window);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU32;
    use std::time::Instant;

    static PIPE_COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Unique pipe name per test so parallel tests and any running Onyx instance
    /// can never collide.
    fn unique_pipe_name(label: &str) -> String {
        let id = PIPE_COUNTER.fetch_add(1, Ordering::SeqCst);
        format!(
            r"\\.\pipe\onyx-test-ipc-{}-{label}-{id}",
            std::process::id()
        )
    }

    #[test]
    fn wire_lines_roundtrip_through_the_parser() {
        let path = PathBuf::from(r"C:\Users\onyx\Documents");
        let open = IpcCommand::OpenPath(Some(path.clone()));
        assert_eq!(
            IpcCommand::from_wire_line(&open.to_wire_line()),
            Some(open.clone())
        );
        assert_eq!(open.to_wire_line(), "OPEN:C:\\Users\\onyx\\Documents\n");

        let show = IpcCommand::ShowWindow;
        assert_eq!(show.to_wire_line(), "SHOW\n");
        assert_eq!(IpcCommand::from_wire_line(&show.to_wire_line()), Some(show));
        // `OpenPath(None)` asks for the same thing as `ShowWindow`.
        assert_eq!(IpcCommand::OpenPath(None).to_wire_line(), "SHOW\n");
        assert_eq!(
            IpcCommand::from_wire_line("OPEN:"),
            Some(IpcCommand::OpenPath(None))
        );
        assert_eq!(
            IpcCommand::from_wire_line("SHOW\r\n"),
            Some(IpcCommand::ShowWindow)
        );
        assert_eq!(IpcCommand::from_wire_line("BOGUS"), None);
        assert_eq!(
            IpcCommand::from_wire_line("OPEN:").unwrap(),
            IpcCommand::OpenPath(None)
        );
    }

    #[test]
    fn default_pipe_name_is_scoped_to_the_user() {
        let name = default_ipc_pipe_name();
        let suffix = name
            .strip_prefix(PIPE_PREFIX)
            .expect("default pipe name keeps the Onyx prefix");
        assert!(!suffix.is_empty());
        assert!(
            suffix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "user fragment must stay ASCII alphanumeric: {suffix}"
        );
        assert_eq!(sanitize_user_fragment(""), "default");
        assert_eq!(sanitize_user_fragment("Zoe\"K\\x "), "ZoeKx");
    }

    #[test]
    fn missing_pipe_returns_false_immediately() {
        let pipe_name = unique_pipe_name("missing");
        let started = Instant::now();
        assert!(!try_send_to_pipe(&pipe_name, Some(Path::new(r"C:\"))));
        assert!(!try_send_to_pipe(&pipe_name, None));
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "a missing pipe must not wait: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn server_delivers_commands_and_shuts_down_cleanly() {
        let pipe_name = unique_pipe_name("roundtrip");
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        let server = start_single_instance_server_at(&pipe_name, move |command| {
            sink.lock().unwrap().push(command);
        })
        .unwrap();
        assert_eq!(server.pipe_name(), pipe_name);

        let path = std::env::temp_dir();
        assert!(try_send_to_pipe(&pipe_name, Some(&path)));
        assert!(try_send_to_pipe(&pipe_name, None));

        let deadline = Instant::now() + Duration::from_secs(5);
        while received.lock().unwrap().len() < 2 {
            assert!(
                Instant::now() < deadline,
                "server did not deliver both commands: {:?}",
                received.lock().unwrap()
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            *received.lock().unwrap(),
            vec![IpcCommand::OpenPath(Some(path)), IpcCommand::ShowWindow]
        );

        let started = Instant::now();
        drop(server);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "server drop must not block: {:?}",
            started.elapsed()
        );
        // The listener released the pipe, so a later launch cold-starts again.
        assert!(!try_send_to_pipe(&pipe_name, None));
    }

    #[test]
    fn second_server_on_the_same_pipe_name_is_rejected() {
        let pipe_name = unique_pipe_name("exclusive");
        let first = start_single_instance_server_at(&pipe_name, |_| {}).unwrap();
        assert!(matches!(
            start_single_instance_server_at(&pipe_name, |_| {}),
            Err(OnyxError::Platform { .. })
        ));
        drop(first);
        // Dropping the owner frees the name for the next process.
        let reopened = start_single_instance_server_at(&pipe_name, |_| {}).unwrap();
        drop(reopened);
    }

    #[test]
    fn focus_window_rejects_foreign_handles() {
        assert!(matches!(focus_window(0), Err(OnyxError::Platform { .. })));
        assert!(matches!(
            focus_window(0xDEAD_BEEF),
            Err(OnyxError::Platform { .. })
        ));
    }
}
