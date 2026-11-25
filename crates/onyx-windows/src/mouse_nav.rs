//! Win32 window subclassing for mouse side buttons (`XBUTTON1` / `XBUTTON2`).
//!
//! Slint's winit backend translates mouse side buttons into pointer events only for the
//! element directly under the cursor, without a window-wide hook. Subclassing the native
//! top-level `HWND` with `SetWindowLongPtrW(GWLP_WNDPROC)` intercepts `WM_XBUTTONDOWN`
//! and `WM_XBUTTONUP` anywhere in the window while forwarding all other messages to the
//! original window procedure via `CallWindowProcW`.

use onyx_core::{OnyxError, OnyxResult};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use windows::Win32::Foundation::{
    ERROR_SUCCESS, GetLastError, HWND, LPARAM, LRESULT, SetLastError, WPARAM,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, GWLP_WNDPROC, GetWindowLongPtrW, IsWindow, SetWindowLongPtrW,
    WM_NCDESTROY, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDPROC, XBUTTON1, XBUTTON2,
};

/// Navigation direction triggered by a mouse side button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseNavDirection {
    /// Mouse button 4 (`XBUTTON1`) — navigate back.
    Back,
    /// Mouse button 5 (`XBUTTON2`) — navigate forward.
    Forward,
}

type MouseNavCallback = Arc<dyn Fn(MouseNavDirection) + Send + Sync>;

struct HookEntry {
    id: u64,
    prev_wndproc: isize,
    callback: Option<MouseNavCallback>,
}

#[derive(Default)]
struct HookRegistry {
    entries: HashMap<isize, HookEntry>,
}

impl HookRegistry {
    fn get_dispatch(&self, hwnd: isize) -> Option<(isize, Option<MouseNavCallback>)> {
        self.entries
            .get(&hwnd)
            .map(|entry| (entry.prev_wndproc, entry.callback.clone()))
    }

    fn has_active_entry(&self, hwnd: isize) -> bool {
        self.entries.contains_key(&hwnd)
    }

    fn update_callback(&mut self, hwnd: isize, id: u64, callback: MouseNavCallback) -> bool {
        if let Some(entry) = self.entries.get_mut(&hwnd) {
            entry.id = id;
            entry.callback = Some(callback);
            true
        } else {
            false
        }
    }

    fn insert(&mut self, hwnd: isize, id: u64, prev_wndproc: isize, callback: MouseNavCallback) {
        self.entries.insert(
            hwnd,
            HookEntry {
                id,
                prev_wndproc,
                callback: Some(callback),
            },
        );
    }

    /// Removes the entry if `expected_id` is `None` or matches the stored entry ID.
    /// Returns the entry's `prev_wndproc` if removed.
    fn remove_matching(&mut self, hwnd: isize, expected_id: Option<u64>) -> Option<isize> {
        let matches = self
            .entries
            .get(&hwnd)
            .is_some_and(|entry| expected_id.is_none_or(|id| entry.id == id));
        if matches {
            self.entries.remove(&hwnd).map(|entry| entry.prev_wndproc)
        } else {
            None
        }
    }

    /// Clears the callback without removing `prev_wndproc` when another subclass sits
    /// on top of ours, turning our wndproc into a transparent pass-through until
    /// `WM_NCDESTROY`.
    fn deactivate_matching(&mut self, hwnd: isize, expected_id: Option<u64>) -> bool {
        if let Some(entry) = self.entries.get_mut(&hwnd)
            && expected_id.is_none_or(|id| entry.id == id)
        {
            entry.callback = None;
            return true;
        }
        false
    }

    fn remove_on_destroy(&mut self, hwnd: isize) -> Option<isize> {
        self.entries.remove(&hwnd).map(|entry| entry.prev_wndproc)
    }
}

static HOOKS: LazyLock<Mutex<HookRegistry>> = LazyLock::new(|| Mutex::new(HookRegistry::default()));
static NEXT_HOOK_ID: AtomicU64 = AtomicU64::new(1);

/// RAII guard for an installed Win32 mouse side-button navigation hook.
///
/// Dropping this guard restores the original window procedure if this installation is still
/// the active hook on the `HWND` and the window still exists.
#[derive(Debug)]
pub struct MouseNavHook {
    hwnd: isize,
    id: u64,
    active: bool,
}

impl MouseNavHook {
    /// Returns the native `HWND` value this hook is attached to.
    pub fn hwnd(&self) -> isize {
        self.hwnd
    }

    /// Explicitly uninstalls this hook, returning `true` if it was still active and removed.
    pub fn uninstall(mut self) -> bool {
        self.uninstall_inner()
    }

    fn uninstall_inner(&mut self) -> bool {
        if !self.active {
            return false;
        }
        self.active = false;
        uninstall_hook_impl(self.hwnd, Some(self.id))
    }
}

impl Drop for MouseNavHook {
    fn drop(&mut self) {
        let _ = self.uninstall_inner();
    }
}

/// Parses the `wParam` of a `WM_XBUTTONDOWN` or `WM_XBUTTONUP` message into a
/// [`MouseNavDirection`].
///
/// Win32 encodes the X-button identifier in the high-order word (`HIWORD(wParam)`):
/// `XBUTTON1` (`0x0001`) maps to [`MouseNavDirection::Back`] and `XBUTTON2` (`0x0002`)
/// maps to [`MouseNavDirection::Forward`].
pub(crate) fn parse_xbutton_wparam(wparam: usize) -> Option<MouseNavDirection> {
    let xbutton = ((wparam >> 16) & 0xFFFF) as u16;
    match xbutton {
        XBUTTON1 => Some(MouseNavDirection::Back),
        XBUTTON2 => Some(MouseNavDirection::Forward),
        _ => None,
    }
}

/// Evaluates whether a window message should be handled by the mouse navigation hook.
///
/// Fires `callback` on `WM_XBUTTONDOWN` for `XBUTTON1`/`XBUTTON2` and swallows both
/// `WM_XBUTTONDOWN` and `WM_XBUTTONUP` (returning `true` so the wndproc returns `TRUE`)
/// to prevent double-firing and suppress default `WM_APPCOMMAND` generation.
/// Returns `false` when no callback is active or the message is unrelated.
fn dispatch_xbutton(msg: u32, wparam: usize, callback: Option<&MouseNavCallback>) -> bool {
    let Some(callback) = callback else {
        return false;
    };
    match msg {
        WM_XBUTTONDOWN => {
            if let Some(direction) = parse_xbutton_wparam(wparam) {
                callback(direction);
                true
            } else {
                false
            }
        }
        WM_XBUTTONUP => parse_xbutton_wparam(wparam).is_some(),
        _ => false,
    }
}

fn hook_wndproc_addr() -> isize {
    mouse_nav_wndproc as *const () as isize
}

/// Installs (or replaces) a Win32 window subclass hook on `hwnd` to intercept
/// `WM_XBUTTONDOWN` and `WM_XBUTTONUP` for mouse back/forward navigation.
///
/// - `XBUTTON1` invokes `callback(MouseNavDirection::Back)`.
/// - `XBUTTON2` invokes `callback(MouseNavDirection::Forward)`.
/// - All other messages are forwarded to the original window procedure via `CallWindowProcW`.
/// - Reinstalling on the same `hwnd` updates the callback in place without stacking
///   duplicate window procedures.
/// - Reinstalling on a new `hwnd` attaches to the new handle cleanly.
pub fn install_mouse_nav_hook<F>(hwnd: isize, callback: F) -> OnyxResult<MouseNavHook>
where
    F: Fn(MouseNavDirection) + Send + Sync + 'static,
{
    let window = crate::own_window(hwnd)?;
    let id = NEXT_HOOK_ID.fetch_add(1, Ordering::Relaxed);
    let callback: MouseNavCallback = Arc::new(callback);
    let our_proc = hook_wndproc_addr();

    let mut registry = HOOKS.lock().map_err(|error| OnyxError::Platform {
        message: format!("Could not lock mouse navigation hook registry: {error}"),
    })?;

    // SAFETY: `window` was validated by `own_window` to belong to the current process.
    let current_proc = unsafe { GetWindowLongPtrW(window, GWLP_WNDPROC) };
    if current_proc == our_proc && registry.has_active_entry(hwnd) {
        registry.update_callback(hwnd, id, callback);
        return Ok(MouseNavHook {
            hwnd,
            id,
            active: true,
        });
    }

    // SAFETY: `SetLastError` is thread-local and always safe to call. `SetWindowLongPtrW`
    // subclasses `window` (owned by the current process) with our `extern "system"` wndproc.
    let prev_proc = unsafe {
        SetLastError(ERROR_SUCCESS);
        SetWindowLongPtrW(window, GWLP_WNDPROC, our_proc)
    };

    if prev_proc == 0 {
        // SAFETY: `GetLastError` reads the calling thread's last-error code.
        let last_err = unsafe { GetLastError() };
        return Err(OnyxError::Platform {
            message: format!(
                "SetWindowLongPtrW(GWLP_WNDPROC) failed for HWND 0x{hwnd:X} (error {})",
                last_err.0
            ),
        });
    }

    if prev_proc == our_proc {
        // Defensive guard against self-chaining if the registry was somehow out of sync.
        return Err(OnyxError::Platform {
            message: format!("HWND 0x{hwnd:X} is already subclassed without a registry entry"),
        });
    }

    registry.insert(hwnd, id, prev_proc, callback);
    Ok(MouseNavHook {
        hwnd,
        id,
        active: true,
    })
}

/// Uninstalls any mouse navigation hook currently attached to `hwnd`.
///
/// Safe to call even if no hook was ever installed or if `hwnd` has already been destroyed.
/// Returns `true` if a registered hook was removed or deactivated.
pub fn uninstall_mouse_nav_hook(hwnd: isize) -> bool {
    uninstall_hook_impl(hwnd, None)
}

fn uninstall_hook_impl(hwnd: isize, expected_id: Option<u64>) -> bool {
    let window = HWND(hwnd as _);
    let our_proc = hook_wndproc_addr();

    // SAFETY: `IsWindow` accepts any HWND value and checks whether the window still exists.
    let window_alive = hwnd != 0 && unsafe { IsWindow(Some(window)) }.as_bool();

    let mut registry = match HOOKS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    if !window_alive {
        return registry.remove_matching(hwnd, expected_id).is_some();
    }

    // SAFETY: `window` was confirmed alive by `IsWindow` above.
    let current_proc = unsafe { GetWindowLongPtrW(window, GWLP_WNDPROC) };
    if current_proc == our_proc {
        let Some(prev_proc) = registry.remove_matching(hwnd, expected_id) else {
            return false;
        };
        // Drop the registry lock before calling into Win32 window procedure state.
        drop(registry);
        // SAFETY: `window` is alive and `prev_proc` is the valid WNDPROC saved during install.
        let _ = unsafe { SetWindowLongPtrW(window, GWLP_WNDPROC, prev_proc) };
        true
    } else {
        // Another subclass sits above us in the chain; keep `prev_wndproc` intact and
        // turn our proc into a pass-through until `WM_NCDESTROY`.
        registry.deactivate_matching(hwnd, expected_id)
    }
}

unsafe extern "system" fn mouse_nav_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let hwnd_key = hwnd.0 as isize;

    if msg == WM_NCDESTROY {
        let prev_proc = {
            let mut registry = match HOOKS.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            registry.remove_on_destroy(hwnd_key)
        };
        return call_prev_or_default(prev_proc, hwnd, msg, wparam, lparam);
    }

    let (prev_proc, callback) = {
        let registry = match HOOKS.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match registry.get_dispatch(hwnd_key) {
            Some((prev, cb)) => (Some(prev), cb),
            None => (None, None),
        }
    };

    // Invoke callback and CallWindowProcW strictly after releasing `HOOKS` to avoid
    // deadlocks on re-entrant messages or hook reinstallation.
    if dispatch_xbutton(msg, wparam.0, callback.as_ref()) {
        // Per Win32 documentation, return TRUE (1) when handling WM_XBUTTONDOWN/WM_XBUTTONUP.
        return LRESULT(1);
    }

    call_prev_or_default(prev_proc, hwnd, msg, wparam, lparam)
}

fn call_prev_or_default(
    prev_proc: Option<isize>,
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if let Some(prev) = prev_proc
        && prev != 0
    {
        // SAFETY: `prev` is a non-null WNDPROC pointer returned by `SetWindowLongPtrW`
        // for this `hwnd`, which matches the `extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT` ABI.
        let prev_fn: WNDPROC = unsafe { std::mem::transmute::<isize, WNDPROC>(prev) };
        // SAFETY: Forwarding the original Win32 message arguments to the saved window procedure.
        return unsafe { CallWindowProcW(prev_fn, hwnd, msg, wparam, lparam) };
    }

    // SAFETY: Fallback to the default window procedure with the exact incoming message arguments.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, HWND_MESSAGE, SendMessageW, WINDOW_EX_STYLE, WINDOW_STYLE,
        WM_APP, WM_LBUTTONDOWN,
    };
    use windows::core::PCWSTR;

    #[test]
    fn parses_xbutton_wparam_variants() {
        // XBUTTON1 in HIWORD, with and without modifier flags in LOWORD.
        assert_eq!(
            parse_xbutton_wparam((XBUTTON1 as usize) << 16),
            Some(MouseNavDirection::Back)
        );
        assert_eq!(
            parse_xbutton_wparam(((XBUTTON1 as usize) << 16) | 0x0024),
            Some(MouseNavDirection::Back)
        );

        // XBUTTON2 in HIWORD, with and without modifier flags in LOWORD.
        assert_eq!(
            parse_xbutton_wparam((XBUTTON2 as usize) << 16),
            Some(MouseNavDirection::Forward)
        );
        assert_eq!(
            parse_xbutton_wparam(((XBUTTON2 as usize) << 16) | 0x0048),
            Some(MouseNavDirection::Forward)
        );

        // Unknown / zero XBUTTON values are ignored.
        assert_eq!(parse_xbutton_wparam(0), None);
        assert_eq!(parse_xbutton_wparam(0x0003 << 16), None);
        assert_eq!(parse_xbutton_wparam(XBUTTON1 as usize), None);
    }

    #[test]
    fn dispatch_fires_once_on_down_and_swallows_up() {
        let events = Arc::new(StdMutex::new(Vec::new()));
        let cb_events = Arc::clone(&events);
        let cb: MouseNavCallback = Arc::new(move |dir| {
            cb_events.lock().unwrap().push(dir);
        });

        let back_wp = (XBUTTON1 as usize) << 16;
        let forward_wp = (XBUTTON2 as usize) << 16;

        assert!(dispatch_xbutton(WM_XBUTTONDOWN, back_wp, Some(&cb)));
        assert!(dispatch_xbutton(WM_XBUTTONUP, back_wp, Some(&cb)));
        assert!(dispatch_xbutton(WM_XBUTTONDOWN, forward_wp, Some(&cb)));
        assert!(dispatch_xbutton(WM_XBUTTONUP, forward_wp, Some(&cb)));

        // Unrelated messages and unknown XBUTTON values are not swallowed.
        assert!(!dispatch_xbutton(WM_LBUTTONDOWN, back_wp, Some(&cb)));
        assert!(!dispatch_xbutton(WM_XBUTTONDOWN, 0, Some(&cb)));
        assert!(!dispatch_xbutton(WM_XBUTTONUP, 0, Some(&cb)));

        // Deactivated callback passes everything through.
        assert!(!dispatch_xbutton(WM_XBUTTONDOWN, back_wp, None));
        assert!(!dispatch_xbutton(WM_XBUTTONUP, back_wp, None));

        assert_eq!(
            *events.lock().unwrap(),
            vec![MouseNavDirection::Back, MouseNavDirection::Forward]
        );
    }

    #[test]
    fn registry_lifecycle_and_generation_guard() {
        let mut reg = HookRegistry::default();
        let cb1: MouseNavCallback = Arc::new(|_| {});
        let cb2: MouseNavCallback = Arc::new(|_| {});

        reg.insert(100, 1, 0x1111, cb1);
        assert!(reg.has_active_entry(100));
        assert_eq!(reg.get_dispatch(100).unwrap().0, 0x1111);

        // Updating callback replaces ID while preserving prev_wndproc.
        assert!(reg.update_callback(100, 2, cb2));
        assert_eq!(reg.get_dispatch(100).unwrap().0, 0x1111);

        // Dropping stale guard (id=1) does not remove the newer installation (id=2).
        assert_eq!(reg.remove_matching(100, Some(1)), None);
        assert!(reg.has_active_entry(100));

        // Deactivating with matching ID clears callback but retains prev_wndproc.
        assert!(reg.deactivate_matching(100, Some(2)));
        let (prev, cb) = reg.get_dispatch(100).unwrap();
        assert_eq!(prev, 0x1111);
        assert!(cb.is_none());

        // WM_NCDESTROY removes entry unconditionally.
        assert_eq!(reg.remove_on_destroy(100), Some(0x1111));
        assert!(!reg.has_active_entry(100));
    }

    #[test]
    fn rejects_invalid_hwnds_and_safe_uninstall_without_install() {
        assert!(matches!(
            install_mouse_nav_hook(0, |_| {}),
            Err(OnyxError::Platform { .. })
        ));
        assert!(matches!(
            install_mouse_nav_hook(0xDEAD_BEEF, |_| {}),
            Err(OnyxError::Platform { .. })
        ));
        assert!(!uninstall_mouse_nav_hook(0));
        assert!(!uninstall_mouse_nav_hook(0xDEAD_BEEF));
    }

    #[test]
    fn real_message_window_subclass_dispatch_reinstall_and_cleanup() {
        let class_name = crate::to_wide_null("STATIC".as_ref());
        // SAFETY: Creating a message-only STATIC window owned by this test thread.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR::null(),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .unwrap();
        let raw_hwnd = hwnd.0 as isize;

        let events = Arc::new(StdMutex::new(Vec::new()));
        let e1 = Arc::clone(&events);
        let hook1 = install_mouse_nav_hook(raw_hwnd, move |dir| {
            e1.lock().unwrap().push((1, dir));
        })
        .unwrap();
        assert_eq!(hook1.hwnd(), raw_hwnd);

        // SAFETY: Sending standard window messages to our own message-only window.
        unsafe {
            let r_down = SendMessageW(
                hwnd,
                WM_XBUTTONDOWN,
                Some(WPARAM((XBUTTON1 as usize) << 16)),
                Some(LPARAM(0)),
            );
            let r_up = SendMessageW(
                hwnd,
                WM_XBUTTONUP,
                Some(WPARAM((XBUTTON1 as usize) << 16)),
                Some(LPARAM(0)),
            );
            assert_eq!(r_down.0, 1);
            assert_eq!(r_up.0, 1);

            // Non-XBUTTON message passes through cleanly without crashing.
            let _ = SendMessageW(hwnd, WM_APP, Some(WPARAM(0)), Some(LPARAM(0)));
        }

        // Reinstall on the same HWND replaces the callback; dropping old guard is a no-op.
        let e2 = Arc::clone(&events);
        let hook2 = install_mouse_nav_hook(raw_hwnd, move |dir| {
            e2.lock().unwrap().push((2, dir));
        })
        .unwrap();
        drop(hook1);

        // SAFETY: Sending XBUTTON2 to verify hook2 is still active after hook1 dropped.
        unsafe {
            let r_down = SendMessageW(
                hwnd,
                WM_XBUTTONDOWN,
                Some(WPARAM((XBUTTON2 as usize) << 16)),
                Some(LPARAM(0)),
            );
            let r_up = SendMessageW(
                hwnd,
                WM_XBUTTONUP,
                Some(WPARAM((XBUTTON2 as usize) << 16)),
                Some(LPARAM(0)),
            );
            assert_eq!(r_down.0, 1);
            assert_eq!(r_up.0, 1);
        }

        assert_eq!(
            *events.lock().unwrap(),
            vec![
                (1, MouseNavDirection::Back),
                (2, MouseNavDirection::Forward)
            ]
        );

        assert!(hook2.uninstall());
        assert!(!uninstall_mouse_nav_hook(raw_hwnd));

        // SAFETY: Destroying the test window we created above.
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}
