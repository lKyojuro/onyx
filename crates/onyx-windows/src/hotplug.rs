//! Live drive hotplug notifications (`WM_DEVICECHANGE`) for USB sticks, SD cards, and
//! network volumes.
//!
//! Windows broadcasts `WM_DEVICECHANGE` only to **top-level** windows. `HWND_MESSAGE`
//! message-only windows never receive the broadcast, so the listener creates a regular
//! `WS_OVERLAPPED` window that is simply never shown.
//!
//! The window lives on its own dedicated thread with its own message loop, and every
//! callback runs on that thread with the registry lock released.

use onyx_core::{OnyxError, OnyxResult};
use std::collections::HashMap;
use std::mem::size_of;
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::JoinHandle;
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DBT_DEVICEARRIVAL, DBT_DEVICEREMOVECOMPLETE, DBT_DEVTYP_VOLUME, DBTF_MEDIA,
    DBTF_NET, DEV_BROADCAST_HDR, DEV_BROADCAST_VOLUME, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GetMessageW, HCURSOR, HICON, IsWindow, MSG, PostMessageW, PostQuitMessage,
    RegisterClassW, TranslateMessage, WINDOW_EX_STYLE, WM_CLOSE, WM_DESTROY, WM_DEVICECHANGE,
    WM_NCDESTROY, WNDCLASS_STYLES, WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::{PCWSTR, w};

/// Which direction a drive change went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DriveChangeKind {
    /// A volume became available (`DBT_DEVICEARRIVAL`).
    Arrived,
    /// A volume went away (`DBT_DEVICEREMOVECOMPLETE`).
    Removed,
}

/// A decoded `WM_DEVICECHANGE` volume notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveChangeEvent {
    pub kind: DriveChangeKind,
    /// Drive letters reported by `dbcv_unitmask`, in ascending order (`'A'..='Z'`).
    pub drive_letters: Vec<char>,
    /// `DBTF_MEDIA`: the volume is removable media.
    pub is_media: bool,
    /// `DBTF_NET`: the volume is a network share.
    pub is_network: bool,
}

type VolumeCallback = Arc<dyn Fn(DriveChangeEvent) + Send + Sync>;

static WATCHERS: LazyLock<Mutex<HashMap<isize, VolumeCallback>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static WINDOW_CLASS: OnceLock<Result<(), String>> = OnceLock::new();

fn watcher_registry() -> MutexGuard<'static, HashMap<isize, VolumeCallback>> {
    WATCHERS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Maps a `DBT_DEVTYP_VOLUME` unit mask onto drive letters.
///
/// Bit `0` is `'A'`, bit `25` is `'Z'`; unset bits and bits above `25` produce no letters.
#[must_use]
pub fn decode_unit_mask(unitmask: u32) -> Vec<char> {
    (0..26u32)
        .filter(|bit| unitmask & (1u32 << bit) != 0)
        .filter_map(|bit| {
            u8::try_from(bit)
                .ok()
                .map(|offset| char::from(b'A' + offset))
        })
        .collect()
}

/// RAII handle for the drive hotplug listener thread and its hidden window.
///
/// Dropping the watcher posts `WM_CLOSE` to the hidden window and joins the thread, so no
/// window, message queue, or thread outlives the watcher.
#[derive(Debug)]
pub struct VolumeHotplugWatcher {
    hwnd: isize,
    thread: Option<JoinHandle<()>>,
}

impl VolumeHotplugWatcher {
    /// Returns the native `HWND` value of the hidden listener window.
    #[must_use]
    pub const fn hwnd(&self) -> isize {
        self.hwnd
    }
}

impl Drop for VolumeHotplugWatcher {
    fn drop(&mut self) {
        if self.hwnd != 0 {
            let window = HWND(self.hwnd as _);
            // SAFETY: `IsWindow` accepts any handle value and only reports whether the window is
            // still alive; `PostMessageW` enqueues `WM_CLOSE` on the watcher thread so that thread
            // performs `DestroyWindow` itself instead of destroying another thread's window.
            unsafe {
                if IsWindow(Some(window)).as_bool() {
                    let _ = PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Starts a background listener that reports drive arrivals and removals to `on_event`.
///
/// Returns only once the hidden window exists, so callers can immediately query
/// [`VolumeHotplugWatcher::hwnd`]. The callback runs on the listener thread.
pub fn start_volume_hotplug_watcher<F>(on_event: F) -> OnyxResult<VolumeHotplugWatcher>
where
    F: Fn(DriveChangeEvent) + Send + Sync + 'static,
{
    let callback: VolumeCallback = Arc::new(on_event);
    let (ready_tx, ready_rx) = mpsc::sync_channel::<OnyxResult<isize>>(1);

    let thread = std::thread::Builder::new()
        .name("onyx-volume-hotplug".to_string())
        .spawn(move || run_hotplug_thread(callback, ready_tx))
        .map_err(|err| OnyxError::Platform {
            message: format!("Could not spawn the volume hotplug listener thread: {err}"),
        })?;

    match ready_rx.recv() {
        Ok(Ok(hwnd)) => Ok(VolumeHotplugWatcher {
            hwnd,
            thread: Some(thread),
        }),
        Ok(Err(err)) => {
            let _ = thread.join();
            Err(err)
        }
        Err(_) => {
            let _ = thread.join();
            Err(OnyxError::Platform {
                message: "The volume hotplug listener thread exited before creating its window"
                    .to_string(),
            })
        }
    }
}

fn run_hotplug_thread(callback: VolumeCallback, ready: SyncSender<OnyxResult<isize>>) {
    let window = match create_hotplug_window() {
        Ok(window) => window,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };
    let key = window.0 as isize;

    // Register before announcing readiness so a caller that messages the window immediately
    // always finds the callback.
    watcher_registry().insert(key, callback);

    if ready.send(Ok(key)).is_err() {
        // The starter dropped the receiver; shut this thread down without leaving a window.
        // SAFETY: `window` was created on this thread, so destroying it here is legal.
        unsafe {
            let _ = DestroyWindow(window);
        }
        watcher_registry().remove(&key);
        return;
    }

    run_message_loop();

    // SAFETY: `window` belongs to this thread; destroying an already destroyed handle is a no-op
    // that simply reports an error.
    unsafe {
        if IsWindow(Some(window)).as_bool() {
            let _ = DestroyWindow(window);
        }
    }
    watcher_registry().remove(&key);
}

fn run_message_loop() {
    let mut msg = MSG::default();
    loop {
        // SAFETY: `msg` is a writable `MSG` buffer and filtering is disabled (`hwnd: None`,
        // `0..0`), so every message queued for this thread is retrieved.
        let result = unsafe { GetMessageW(&raw mut msg, None, 0, 0) }.0;
        // 0 means `WM_QUIT`; -1 means the queue failed. Both end the loop.
        if result == 0 || result == -1 {
            break;
        }
        // SAFETY: `msg` was populated by `GetMessageW` above and is only read here.
        unsafe {
            let _ = TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
}

fn window_class_name() -> PCWSTR {
    w!("OnyxVolumeHotplugWatcher")
}

fn register_window_class() -> OnyxResult<()> {
    // SAFETY: `GetModuleHandleW(None)` returns the executable module handle for this process.
    let instance = unsafe { GetModuleHandleW(None) }.map_err(|err| OnyxError::Platform {
        message: format!("GetModuleHandleW failed while creating the hotplug window: {err}"),
    })?;

    let class = WNDCLASSW {
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(hotplug_wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: HINSTANCE(instance.0),
        hIcon: HICON::default(),
        hCursor: HCURSOR::default(),
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: window_class_name(),
    };

    // SAFETY: `class` is fully initialized, its wndproc is a valid `extern "system"` function, and
    // its class name points at a `'static` wide string literal.
    let atom = unsafe { RegisterClassW(&raw const class) };
    if atom == 0 {
        // SAFETY: `GetLastError` reads the calling thread's last-error value. A duplicate class
        // registration is expected when several watchers are created in one process.
        let last = unsafe { GetLastError() };
        if last != ERROR_CLASS_ALREADY_EXISTS {
            return Err(OnyxError::Platform {
                message: format!(
                    "RegisterClassW failed for the hotplug window (error {})",
                    last.0
                ),
            });
        }
    }
    Ok(())
}

fn ensure_window_class() -> OnyxResult<()> {
    let outcome =
        WINDOW_CLASS.get_or_init(|| register_window_class().map_err(|err| err.to_string()));
    match outcome {
        Ok(()) => Ok(()),
        Err(message) => Err(OnyxError::Platform {
            message: message.clone(),
        }),
    }
}

fn create_hotplug_window() -> OnyxResult<HWND> {
    ensure_window_class()?;

    // SAFETY: `GetModuleHandleW(None)` returns the executable module handle for this process.
    let instance = unsafe { GetModuleHandleW(None) }.map_err(|err| OnyxError::Platform {
        message: format!("GetModuleHandleW failed while creating the hotplug window: {err}"),
    })?;

    // SAFETY: The class is registered above. `WS_OVERLAPPED` without `WS_VISIBLE` and with a null
    // parent produces a hidden top-level window, which is exactly what `WM_DEVICECHANGE`
    // broadcasts require (`HWND_MESSAGE` windows never receive them).
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            window_class_name(),
            window_class_name(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(HINSTANCE(instance.0)),
            None,
        )
    }
    .map_err(|err| OnyxError::Platform {
        message: format!("CreateWindowExW failed for the hotplug window: {err}"),
    })
}

/// Decodes a `WM_DEVICECHANGE` payload into a [`DriveChangeEvent`].
///
/// # Safety
///
/// `lparam` must be `0` or point at a readable `DEV_BROADCAST_*` header whose `dbch_size` field
/// describes the real length of the buffer behind the pointer.
unsafe fn parse_volume_event(action: u32, lparam: isize) -> Option<DriveChangeEvent> {
    let kind = match action {
        DBT_DEVICEARRIVAL => DriveChangeKind::Arrived,
        DBT_DEVICEREMOVECOMPLETE => DriveChangeKind::Removed,
        _ => return None,
    };
    if lparam == 0 {
        return None;
    }

    // SAFETY: The caller guarantees `lparam` points at a readable `DEV_BROADCAST_HDR`.
    let header = unsafe { (lparam as *const DEV_BROADCAST_HDR).read_unaligned() };
    if header.dbch_devicetype != DBT_DEVTYP_VOLUME {
        return None;
    }
    if (header.dbch_size as usize) < size_of::<DEV_BROADCAST_VOLUME>() {
        // Truncated or unexpected payload: refuse to read past what the sender declared.
        return None;
    }

    // SAFETY: The size check above confirms the buffer covers a full `DEV_BROADCAST_VOLUME`.
    let volume = unsafe { (lparam as *const DEV_BROADCAST_VOLUME).read_unaligned() };
    Some(DriveChangeEvent {
        kind,
        drive_letters: decode_unit_mask(volume.dbcv_unitmask),
        is_media: volume.dbcv_flags.0 & DBTF_MEDIA.0 != 0,
        is_network: volume.dbcv_flags.0 & DBTF_NET.0 != 0,
    })
}

fn dispatch_event(hwnd: HWND, event: DriveChangeEvent) {
    // Clone the callback out and drop the lock before invoking it: user callbacks may start or
    // stop watchers, which would deadlock against a held registry lock.
    let callback = watcher_registry().get(&(hwnd.0 as isize)).cloned();
    let Some(callback) = callback else {
        return;
    };
    // A panicking callback must never unwind across the FFI boundary into `DispatchMessageW`.
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| callback(event)));
}

unsafe extern "system" fn hotplug_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DEVICECHANGE => {
            // SAFETY: Device change messages carry either a null `lparam` or a `DEV_BROADCAST_HDR`
            // pointer whose validity is checked inside `parse_volume_event`.
            if let Some(event) = unsafe { parse_volume_event(wparam.0 as u32, lparam.0) } {
                dispatch_event(hwnd, event);
            }
            // The return value is ignored for arrival/removal-complete notifications.
            LRESULT(1)
        }
        WM_CLOSE => {
            // SAFETY: `hwnd` is the live window this procedure belongs to.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: `PostQuitMessage` takes only an exit code and is safe on the owning thread.
            unsafe {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            watcher_registry().remove(&(hwnd.0 as isize));
            // SAFETY: Forwarding the original message arguments to the default window procedure.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        _ => {
            // SAFETY: Forwarding the original message arguments to the default window procedure.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use windows::Win32::UI::WindowsAndMessaging::{
        DEV_BROADCAST_HDR_DEVICE_TYPE, DEV_BROADCAST_VOLUME_FLAGS, SendMessageW,
    };

    #[test]
    fn decodes_unit_masks_into_drive_letters() {
        assert_eq!(decode_unit_mask(0), Vec::<char>::new());
        assert_eq!(decode_unit_mask(0b101), vec!['A', 'C']);
        assert_eq!(decode_unit_mask(1 << 25), vec!['Z']);
        assert_eq!(decode_unit_mask(0b11), vec!['A', 'B']);
        assert_eq!(decode_unit_mask(1 << 4), vec!['E']);
        // Bits above 'Z' map to no drive letter at all.
        assert_eq!(decode_unit_mask(0x0400_0000), Vec::<char>::new());
    }

    #[test]
    fn parses_volume_broadcasts_and_ignores_other_payloads() {
        let volume = DEV_BROADCAST_VOLUME {
            dbcv_size: u32::try_from(size_of::<DEV_BROADCAST_VOLUME>()).unwrap(),
            dbcv_devicetype: DBT_DEVTYP_VOLUME.0,
            dbcv_reserved: 0,
            dbcv_unitmask: 1 << 4,
            dbcv_flags: DEV_BROADCAST_VOLUME_FLAGS(DBTF_MEDIA.0),
        };
        let ptr = (&raw const volume).cast::<DEV_BROADCAST_HDR>() as isize;

        // SAFETY: `ptr` points at a fully initialized `DEV_BROADCAST_VOLUME` on this stack frame.
        let arrived = unsafe { parse_volume_event(DBT_DEVICEARRIVAL, ptr) }.unwrap();
        assert_eq!(
            arrived,
            DriveChangeEvent {
                kind: DriveChangeKind::Arrived,
                drive_letters: vec!['E'],
                is_media: true,
                is_network: false,
            }
        );

        // SAFETY: Same stack payload as above.
        let removed = unsafe { parse_volume_event(DBT_DEVICEREMOVECOMPLETE, ptr) }.unwrap();
        assert_eq!(removed.kind, DriveChangeKind::Removed);

        let network_volume = DEV_BROADCAST_VOLUME {
            dbcv_flags: DEV_BROADCAST_VOLUME_FLAGS(DBTF_NET.0),
            ..volume
        };
        let network_ptr = (&raw const network_volume).cast::<DEV_BROADCAST_HDR>() as isize;
        // SAFETY: `network_ptr` points at a fully initialized `DEV_BROADCAST_VOLUME`.
        let networked = unsafe { parse_volume_event(DBT_DEVICEARRIVAL, network_ptr) }.unwrap();
        assert!(networked.is_network);
        assert!(!networked.is_media);

        // SAFETY: Null `lparam` is explicitly allowed by the function's contract.
        assert!(unsafe { parse_volume_event(DBT_DEVICEARRIVAL, 0) }.is_none());
        // SAFETY: Same valid payload, but the action is not an arrival/removal.
        assert!(unsafe { parse_volume_event(0x8001, ptr) }.is_none());

        // Non-volume device types are ignored even when the action matches.
        let header = DEV_BROADCAST_HDR {
            dbch_size: u32::try_from(size_of::<DEV_BROADCAST_HDR>()).unwrap(),
            dbch_devicetype: DEV_BROADCAST_HDR_DEVICE_TYPE(3),
            dbch_reserved: 0,
        };
        // SAFETY: `header_ptr` points at a fully initialized `DEV_BROADCAST_HDR` on this frame.
        let non_volume = unsafe {
            parse_volume_event(DBT_DEVICEARRIVAL, (&raw const header) as *const _ as isize)
        };
        assert!(non_volume.is_none());

        // A payload that declares itself smaller than a volume struct is rejected.
        let truncated = DEV_BROADCAST_HDR {
            dbch_size: u32::try_from(size_of::<DEV_BROADCAST_HDR>()).unwrap(),
            dbch_devicetype: DBT_DEVTYP_VOLUME,
            dbch_reserved: 0,
        };
        // SAFETY: The payload is a header whose declared size is too small for a volume, which the
        // parser must detect before reading further.
        let short = unsafe {
            parse_volume_event(
                DBT_DEVICEARRIVAL,
                (&raw const truncated) as *const _ as isize,
            )
        };
        assert!(short.is_none());
    }

    #[test]
    fn live_watcher_receives_synthetic_broadcasts_and_shuts_down_cleanly() {
        let dir = tempfile::TempDir::new().unwrap();
        let events = Arc::new(StdMutex::new(Vec::new()));
        let sink = Arc::clone(&events);

        let watcher = start_volume_hotplug_watcher(move |event| {
            sink.lock().unwrap().push(event);
        })
        .expect("the hotplug watcher should start");

        let hwnd = HWND(watcher.hwnd() as _);
        assert_ne!(watcher.hwnd(), 0);

        let mut volume = DEV_BROADCAST_VOLUME {
            dbcv_size: u32::try_from(size_of::<DEV_BROADCAST_VOLUME>()).unwrap(),
            dbcv_devicetype: DBT_DEVTYP_VOLUME.0,
            dbcv_reserved: 0,
            dbcv_unitmask: 1 << 4,
            dbcv_flags: DEV_BROADCAST_VOLUME_FLAGS(DBTF_MEDIA.0),
        };
        let payload = (&raw mut volume).cast::<core::ffi::c_void>() as isize;

        // SAFETY: `payload` points at the live `DEV_BROADCAST_VOLUME` above, and `SendMessageW`
        // marshals into the watcher thread which only reads it during the call.
        let arrival = unsafe {
            SendMessageW(
                hwnd,
                WM_DEVICECHANGE,
                Some(WPARAM(DBT_DEVICEARRIVAL as usize)),
                Some(LPARAM(payload)),
            )
        };
        assert_eq!(arrival.0, 1);

        // SAFETY: Same payload, removal-complete notification.
        let removal = unsafe {
            SendMessageW(
                hwnd,
                WM_DEVICECHANGE,
                Some(WPARAM(DBT_DEVICEREMOVECOMPLETE as usize)),
                Some(LPARAM(payload)),
            )
        };
        assert_eq!(removal.0, 1);

        assert_eq!(
            *events.lock().unwrap(),
            vec![
                DriveChangeEvent {
                    kind: DriveChangeKind::Arrived,
                    drive_letters: vec!['E'],
                    is_media: true,
                    is_network: false,
                },
                DriveChangeEvent {
                    kind: DriveChangeKind::Removed,
                    drive_letters: vec!['E'],
                    is_media: true,
                    is_network: false,
                },
            ]
        );

        drop(watcher);

        // The watcher thread is joined by `Drop`, so no further events can arrive.
        assert_eq!(events.lock().unwrap().len(), 2);
        assert!(dir.path().exists());
    }
}
