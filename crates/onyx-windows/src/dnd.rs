//! Native Windows external file Drag & Drop (`WM_DROPFILES` inbound + OLE `DoDragDrop` outbound).

use crate::own_window;
use onyx_core::{OnyxError, OnyxResult, normalize_display_path};
use std::ffi::{OsStr, OsString, c_void};
use std::marker::PhantomData;
use std::mem::{ManuallyDrop, size_of};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, DV_E_DVASPECT,
    DV_E_FORMATETC, DV_E_TYMED, E_NOTIMPL, E_POINTER, FALSE, GlobalFree, HGLOBAL, HWND, LPARAM,
    LRESULT, OLE_E_ADVISENOTSUPPORTED, POINT, RPC_E_CHANGED_MODE, S_FALSE, S_OK, TRUE, WPARAM,
};
use windows::Win32::System::Com::{
    DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl,
    IEnumFORMATETC, IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalUnlock,
};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE, DoDragDrop,
    IDropSource, IDropSource_Impl, OleInitialize, OleUninitialize,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MK_RBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, ReleaseCapture, VK_LBUTTON};
use windows::Win32::UI::Shell::{
    DROPFILES, DefSubclassProc, DragAcceptFiles, DragFinish, DragQueryFileW, DragQueryPoint, HDROP,
    RemoveWindowSubclass, SHCreateStdEnumFmtEtc, SetWindowSubclass,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowThreadProcessId, WM_DROPFILES, WM_NCDESTROY,
};
use windows::core::{BOOL, HRESULT, Ref, implement};

static NEXT_SUBCLASS_ID: AtomicUsize = AtomicUsize::new(1);

/// Inbound file drop event delivered when external files are dropped onto an Onyx window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropEvent {
    /// Dropped filesystem paths extracted from the `HDROP` handle.
    pub paths: Vec<PathBuf>,
    /// Client-area X coordinate of the drop point.
    pub x: i32,
    /// Client-area Y coordinate of the drop point.
    pub y: i32,
}

/// Outcome of an outbound external file drag operation initiated via [`start_external_file_drag`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragOutcome {
    /// Target accepted the files as a copy (`DROPEFFECT_COPY`).
    Copied,
    /// Target accepted the files as a move (`DROPEFFECT_MOVE`).
    Moved,
    /// Drag was cancelled or dropped with `DROPEFFECT_NONE`.
    Cancelled,
}

struct DropCallbackState {
    on_drop: Arc<dyn Fn(DropEvent) + Send + Sync + 'static>,
    detached: AtomicBool,
    closed: AtomicBool,
}

impl DropCallbackState {
    /// Atomically releases the `Arc` reference held by the window subclass procedure at most once.
    ///
    /// This guarantees that whether teardown happens via `DropTargetGuard::drop` or via
    /// `WM_NCDESTROY` (or both in either order), the raw `Arc` pointer stored in `dwRefData`
    /// is reclaimed exactly once with zero use-after-free or double-free.
    unsafe fn release_subclass_ref(raw_ptr: *const Self) {
        if raw_ptr.is_null() {
            return;
        }
        // SAFETY: Caller guarantees `raw_ptr` points to a live `DropCallbackState` managed by `Arc`.
        let state = unsafe { &*raw_ptr };
        if state
            .detached
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // SAFETY: The `compare_exchange` succeeded once, so we own the single `Arc::into_raw`
            // reference created during `register_file_drop_target`.
            unsafe {
                drop(Arc::from_raw(raw_ptr));
            }
        }
    }
}

/// RAII guard for a registered window file-drop target subclass.
///
/// On drop, revokes `DragAcceptFiles(hwnd, false)`, removes the window subclass, and safely
/// releases the callback closure state.
pub struct DropTargetGuard {
    hwnd: HWND,
    subclass_id: usize,
    state: Arc<DropCallbackState>,
    raw_state_ptr: *const DropCallbackState,
}

impl std::fmt::Debug for DropTargetGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DropTargetGuard")
            .field("hwnd", &self.hwnd.0)
            .field("subclass_id", &self.subclass_id)
            .finish()
    }
}

impl Drop for DropTargetGuard {
    fn drop(&mut self) {
        self.state.closed.store(true, Ordering::Release);
        if !self.state.detached.load(Ordering::Acquire) {
            // SAFETY: `self.hwnd` was validated as a window on the current thread. Calling
            // `DragAcceptFiles` and `RemoveWindowSubclass` on teardown is safe even if the
            // window is already closing.
            let removed = unsafe {
                DragAcceptFiles(self.hwnd, false);
                RemoveWindowSubclass(self.hwnd, Some(drop_target_subclass_proc), self.subclass_id)
            };
            if removed.as_bool() {
                // SAFETY: `RemoveWindowSubclass` succeeded, so the subclass procedure will no
                // longer be invoked with `self.raw_state_ptr`. Reclaim the subclass `Arc` reference.
                unsafe {
                    DropCallbackState::release_subclass_ref(self.raw_state_ptr);
                }
            } else if !self.state.detached.load(Ordering::Acquire) {
                tracing::error!(
                    hwnd = self.hwnd.0 as isize,
                    subclass_id = self.subclass_id,
                    "RemoveWindowSubclass failed during DropTargetGuard::drop; deferring Arc release to WM_NCDESTROY"
                );
            }
        }
    }
}

/// Registers `hwnd` to accept external file drops via `WM_DROPFILES`, invoking `on_drop`
/// whenever the user drops one or more files from Explorer or another application.
///
/// Returns an RAII [`DropTargetGuard`] that unregisters the drop target when dropped.
pub fn register_file_drop_target<F>(hwnd: isize, on_drop: F) -> OnyxResult<DropTargetGuard>
where
    F: Fn(DropEvent) + Send + Sync + 'static,
{
    let window = own_window(hwnd)?;
    // SAFETY: `GetWindowThreadProcessId` and `GetCurrentThreadId` are safe to call on any thread.
    let (window_thread, current_thread) =
        unsafe { (GetWindowThreadProcessId(window, None), GetCurrentThreadId()) };
    if window_thread == 0 || window_thread != current_thread {
        return Err(OnyxError::Platform {
            message: format!(
                "HWND 0x{hwnd:X} belongs to thread {window_thread}, not calling thread {current_thread}"
            ),
        });
    }

    let subclass_id = NEXT_SUBCLASS_ID.fetch_add(1, Ordering::Relaxed);

    let state = Arc::new(DropCallbackState {
        on_drop: Arc::new(on_drop),
        detached: AtomicBool::new(false),
        closed: AtomicBool::new(false),
    });
    let raw_state_ptr = Arc::into_raw(Arc::clone(&state));

    // SAFETY: `window` belongs to the calling thread (`own_window` + thread check),
    // `drop_target_subclass_proc` is a valid subclass procedure, and `raw_state_ptr` is an
    // `Arc`-backed pointer that stays valid until `release_subclass_ref` is called on successful
    // subclass removal or `WM_NCDESTROY`.
    let installed = unsafe {
        SetWindowSubclass(
            window,
            Some(drop_target_subclass_proc),
            subclass_id,
            raw_state_ptr as usize,
        )
    };

    if !installed.as_bool() {
        state.closed.store(true, Ordering::Release);
        // SAFETY: `SetWindowSubclass` failed, so reclaim the cloned `Arc` immediately.
        unsafe {
            DropCallbackState::release_subclass_ref(raw_state_ptr);
        }
        return Err(OnyxError::Platform {
            message: format!("SetWindowSubclass failed for HWND 0x{hwnd:X}"),
        });
    }

    // SAFETY: `window` is a valid HWND in the current process and thread.
    unsafe {
        DragAcceptFiles(window, true);
    }

    Ok(DropTargetGuard {
        hwnd: window,
        subclass_id,
        state,
        raw_state_ptr,
    })
}

struct HdropFinishGuard(HDROP);

impl Drop for HdropFinishGuard {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the live `HDROP` handle from `WM_DROPFILES` and is released once.
        unsafe {
            DragFinish(self.0);
        }
    }
}

unsafe extern "system" fn drop_target_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass_id: usize,
    ref_data: usize,
) -> LRESULT {
    let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Option<LRESULT> {
        let raw_state_ptr = ref_data as *const DropCallbackState;

        if msg == WM_DROPFILES && !raw_state_ptr.is_null() {
            // Increment the Arc strong count for the duration of callback execution so that even
            // if `on_drop` drops the `DropTargetGuard`, `state` stays alive throughout this call.
            // SAFETY: `raw_state_ptr` is valid while the subclass is installed and not detached.
            let state = unsafe {
                Arc::increment_strong_count(raw_state_ptr);
                Arc::from_raw(raw_state_ptr)
            };

            let hdrop = HDROP(wparam.0 as *mut c_void);
            let _finish_guard = HdropFinishGuard(hdrop);

            if state.closed.load(Ordering::Acquire) || state.detached.load(Ordering::Acquire) {
                return Some(LRESULT(0));
            }

            let mut pt = POINT::default();
            // SAFETY: `hdrop` is a valid drop handle from `WM_DROPFILES` and `pt` is writable.
            let _ = unsafe { DragQueryPoint(hdrop, &raw mut pt) };

            // SAFETY: Passing `0xFFFF_FFFF` queries the number of dropped files in `hdrop`.
            let file_count = unsafe { DragQueryFileW(hdrop, 0xFFFF_FFFF, None) };
            let mut paths = Vec::with_capacity(file_count as usize);

            for index in 0..file_count {
                // SAFETY: Passing `None` queries the required character count (excluding null terminator).
                let char_len = unsafe { DragQueryFileW(hdrop, index, None) };
                if char_len == 0 {
                    continue;
                }
                let mut buf = vec![0u16; (char_len as usize) + 1];
                // SAFETY: `buf` has capacity for `char_len + 1` UTF-16 code units.
                let copied = unsafe { DragQueryFileW(hdrop, index, Some(&mut buf)) };
                if copied > 0 {
                    let os_str = OsString::from_wide(&buf[..copied as usize]);
                    paths.push(PathBuf::from(os_str));
                }
            }

            if !paths.is_empty() && !state.closed.load(Ordering::Acquire) {
                let callback = Arc::clone(&state.on_drop);
                let event = DropEvent {
                    paths,
                    x: pt.x,
                    y: pt.y,
                };
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    callback(event);
                }))
                .is_err()
                {
                    tracing::error!(
                        hwnd = hwnd.0 as isize,
                        "panic caught in WM_DROPFILES drop target callback"
                    );
                }
            }
            return Some(LRESULT(0));
        }

        if msg == WM_NCDESTROY {
            // SAFETY: Window is being destroyed; remove our subclass and release its Arc reference once.
            unsafe {
                let _ = RemoveWindowSubclass(hwnd, Some(drop_target_subclass_proc), subclass_id);
                if !raw_state_ptr.is_null() {
                    (*raw_state_ptr).closed.store(true, Ordering::Release);
                }
                DropCallbackState::release_subclass_ref(raw_state_ptr);
            }
        }

        None
    }));

    match handled {
        Ok(Some(result)) => result,
        Ok(None) => {
            // SAFETY: Forward all other messages through the standard window subclass chain.
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
        Err(_) => {
            tracing::error!(
                hwnd = hwnd.0 as isize,
                msg,
                "panic caught in drop_target_subclass_proc; forwarding to DefSubclassProc"
            );
            // SAFETY: Forward message to default subclass procedure after catching panic across FFI.
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
    }
}

/// Encodes a non-empty slice of filesystem paths into a raw `CF_HDROP` (`DROPFILES` + double-null
/// UTF-16) byte buffer.
pub fn encode_cf_hdrop(paths: &[PathBuf]) -> OnyxResult<Vec<u8>> {
    if paths.is_empty() {
        return Err(OnyxError::InvalidPath {
            message: "CF_HDROP requires at least one file path".to_string(),
        });
    }

    let mut wide_units: Vec<u16> = Vec::new();
    for path in paths {
        if path.as_os_str().is_empty() || path.as_os_str().encode_wide().any(|u| u == 0) {
            return Err(OnyxError::InvalidPath {
                message: "CF_HDROP path must not be empty or contain interior NUL characters"
                    .to_string(),
            });
        }
        let display = normalize_display_path(path);
        wide_units.extend(OsStr::new(&display).encode_wide());
        wide_units.push(0);
    }
    wide_units.push(0);

    let header = DROPFILES {
        pFiles: size_of::<DROPFILES>() as u32,
        pt: POINT { x: 0, y: 0 },
        fNC: FALSE,
        fWide: TRUE,
    };

    let total_bytes = size_of::<DROPFILES>() + wide_units.len() * size_of::<u16>();
    let mut buffer = vec![0u8; total_bytes];

    // SAFETY: `buffer` has `total_bytes` capacity; `DROPFILES` fits in the first `size_of::<DROPFILES>()` bytes.
    unsafe {
        ptr::copy_nonoverlapping(
            (&raw const header).cast::<u8>(),
            buffer.as_mut_ptr(),
            size_of::<DROPFILES>(),
        );
    }

    let mut cursor = size_of::<DROPFILES>();
    for unit in wide_units {
        let le = unit.to_le_bytes();
        buffer[cursor] = le[0];
        buffer[cursor + 1] = le[1];
        cursor += 2;
    }

    Ok(buffer)
}

/// Decodes a raw `CF_HDROP` (`DROPFILES` + double-null list) byte buffer into `Vec<PathBuf>`.
pub fn decode_cf_hdrop(bytes: &[u8]) -> OnyxResult<Vec<PathBuf>> {
    if bytes.len() < size_of::<DROPFILES>() {
        return Err(OnyxError::InvalidPath {
            message: "CF_HDROP buffer is smaller than DROPFILES header".to_string(),
        });
    }

    // SAFETY: `bytes.len() >= size_of::<DROPFILES>()`, so an unaligned read of `DROPFILES` is in-bounds.
    let header: DROPFILES = unsafe { ptr::read_unaligned(bytes.as_ptr().cast::<DROPFILES>()) };
    let offset = header.pFiles as usize;
    if offset < size_of::<DROPFILES>() || offset >= bytes.len() {
        return Err(OnyxError::InvalidPath {
            message: format!("CF_HDROP pFiles offset {offset} is out of bounds"),
        });
    }

    let payload = &bytes[offset..];
    let mut paths = Vec::new();

    if header.fWide.as_bool() {
        let mut units = Vec::with_capacity(payload.len() / 2);
        for pair in payload.chunks_exact(2) {
            units.push(u16::from_le_bytes([pair[0], pair[1]]));
        }
        let mut start = 0usize;
        while start < units.len() {
            let rel_end = units[start..]
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(units.len() - start);
            if rel_end == 0 {
                break;
            }
            let os = OsString::from_wide(&units[start..start + rel_end]);
            paths.push(PathBuf::from(os));
            start += rel_end + 1;
        }
    } else {
        let mut start = 0usize;
        while start < payload.len() {
            let rel_end = payload[start..]
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(payload.len() - start);
            if rel_end == 0 {
                break;
            }
            let s = String::from_utf8_lossy(&payload[start..start + rel_end]);
            paths.push(PathBuf::from(s.into_owned()));
            start += rel_end + 1;
        }
    }

    if paths.is_empty() {
        return Err(OnyxError::InvalidPath {
            message: "CF_HDROP buffer contained no file paths".to_string(),
        });
    }

    Ok(paths)
}

/// RAII guard for `OleInitialize` / `OleUninitialize` on the calling STA thread.
///
/// Intentionally `!Send` and `!Sync` because OLE initialization and uninitialization must run
/// on the same OS thread.
struct OleGuard {
    should_uninit: bool,
    _not_send_sync: PhantomData<*mut ()>,
}

impl OleGuard {
    fn init() -> OnyxResult<Self> {
        // SAFETY: `OleInitialize(None)` is safe to invoke on an STA thread and pairs with `OleUninitialize`.
        match unsafe { OleInitialize(None) } {
            Ok(()) => Ok(Self {
                should_uninit: true,
                _not_send_sync: PhantomData,
            }),
            Err(err) if err.code() == RPC_E_CHANGED_MODE => Err(OnyxError::Platform {
                message: "OLE drag-and-drop requires an STA thread (RPC_E_CHANGED_MODE)"
                    .to_string(),
            }),
            Err(err) => Err(OnyxError::Platform {
                message: format!("OleInitialize failed: {err}"),
            }),
        }
    }
}

impl Drop for OleGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            // SAFETY: Paired strictly with a successful `OleInitialize` on the same thread.
            unsafe {
                OleUninitialize();
            }
        }
    }
}

fn hdrop_formatetc() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

fn matches_hdrop_format(fmt: &FORMATETC) -> Result<(), HRESULT> {
    if fmt.cfFormat != CF_HDROP.0 {
        return Err(DV_E_FORMATETC);
    }
    if (fmt.dwAspect & DVASPECT_CONTENT.0) == 0 {
        return Err(DV_E_DVASPECT);
    }
    if (fmt.tymed & (TYMED_HGLOBAL.0 as u32)) == 0 {
        return Err(DV_E_TYMED);
    }
    Ok(())
}

fn alloc_hglobal_with_bytes(bytes: &[u8]) -> windows::core::Result<HGLOBAL> {
    // SAFETY: `GlobalAlloc` with `GMEM_MOVEABLE | GMEM_ZEROINIT` allocates a movable global memory block.
    let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len()) }?;
    // SAFETY: `hglobal` was just allocated with `bytes.len()` bytes.
    let locked = unsafe { GlobalLock(hglobal) };
    if locked.is_null() {
        // SAFETY: Reclaim `hglobal` on lock failure before returning error.
        let _ = unsafe { GlobalFree(Some(hglobal)) };
        return Err(windows::core::Error::from_thread());
    }
    // SAFETY: `locked` points to at least `bytes.len()` writable bytes; unlocked immediately after copy.
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), locked.cast::<u8>(), bytes.len());
        let _ = GlobalUnlock(hglobal);
    }
    Ok(hglobal)
}

#[implement(IDropSource)]
struct FileDropSource;

impl IDropSource_Impl for FileDropSource_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        if fescapepressed.as_bool() || (grfkeystate & MK_RBUTTON) != MODIFIERKEYS_FLAGS(0) {
            return DRAGDROP_S_CANCEL;
        }
        if (grfkeystate & MK_LBUTTON) == MODIFIERKEYS_FLAGS(0) {
            return DRAGDROP_S_DROP;
        }
        S_OK
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

#[implement(IDataObject)]
struct FileDataObject {
    hdrop_payload: Vec<u8>,
}

impl IDataObject_Impl for FileDataObject_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        if pformatetcin.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: `pformatetcin` was verified non-null above.
        let fmt = unsafe { &*pformatetcin };
        if let Err(hr) = matches_hdrop_format(fmt) {
            return Err(hr.into());
        }

        let hglobal = alloc_hglobal_with_bytes(&self.hdrop_payload)?;
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: hglobal },
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }

    fn GetDataHere(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *mut STGMEDIUM,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        if pformatetc.is_null() {
            return E_POINTER;
        }
        // SAFETY: `pformatetc` was verified non-null above.
        let fmt = unsafe { &*pformatetc };
        match matches_hdrop_format(fmt) {
            Ok(()) => S_OK,
            Err(hr) => hr,
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        _pformatectin: *const FORMATETC,
        pformatetcout: *mut FORMATETC,
    ) -> HRESULT {
        if !pformatetcout.is_null() {
            // SAFETY: Caller provided writable `pformatetcout`.
            unsafe {
                (*pformatetcout).ptd = ptr::null_mut();
            }
        }
        E_NOTIMPL
    }

    fn SetData(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *const STGMEDIUM,
        _frelease: BOOL,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        if dwdirection == (DATADIR_GET.0 as u32) {
            let formats = [hdrop_formatetc()];
            // SAFETY: `formats` is a valid slice of `FORMATETC` structs copied by `SHCreateStdEnumFmtEtc`.
            unsafe { SHCreateStdEnumFmtEtc(&formats) }
        } else {
            Err(E_NOTIMPL.into())
        }
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: Ref<'_, IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// Initiates an outbound synchronous Windows OLE drag-and-drop loop for `paths` using a
/// `CF_HDROP` [`IDataObject`] and [`IDropSource`].
///
/// Must be called on an STA thread (typically the UI thread when a pointer drag threshold is crossed).
pub fn start_external_file_drag(paths: &[PathBuf]) -> OnyxResult<DragOutcome> {
    let hdrop_payload = encode_cf_hdrop(paths)?;
    let _ole = OleGuard::init()?;
    // SAFETY: VK_LBUTTON is a valid virtual-key code and this call reads the current input state.
    if unsafe { GetAsyncKeyState(i32::from(VK_LBUTTON.0)) } >= 0 {
        return Ok(DragOutcome::Cancelled);
    }
    // SAFETY: The caller uses the window's STA thread. ReleaseCapture only releases mouse capture
    // owned by this thread, allowing OLE to take over the active drag.
    let _ = unsafe { ReleaseCapture() };

    let data_object: IDataObject = FileDataObject { hdrop_payload }.into();
    let drop_source: IDropSource = FileDropSource.into();
    let mut effect = DROPEFFECT_NONE;

    // SAFETY: OLE is initialized on this STA thread via `_ole`, `data_object` and `drop_source`
    // are valid COM interfaces, and `effect` points to valid stack storage.
    let hr = unsafe {
        DoDragDrop(
            &data_object,
            &drop_source,
            DROPEFFECT_COPY | DROPEFFECT_MOVE,
            &raw mut effect,
        )
    };

    if hr == DRAGDROP_S_DROP {
        if (effect & DROPEFFECT_MOVE) != DROPEFFECT_NONE {
            Ok(DragOutcome::Moved)
        } else if (effect & DROPEFFECT_COPY) != DROPEFFECT_NONE {
            Ok(DragOutcome::Copied)
        } else {
            Ok(DragOutcome::Cancelled)
        }
    } else if hr == DRAGDROP_S_CANCEL || hr == S_OK || hr == S_FALSE {
        Ok(DragOutcome::Cancelled)
    } else {
        Err(OnyxError::Platform {
            message: format!("DoDragDrop failed with HRESULT 0x{:08X}", hr.0 as u32),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use windows::Win32::System::Memory::GlobalSize;
    use windows::Win32::System::Ole::ReleaseStgMedium;

    #[test]
    fn rejects_invalid_hwnd_on_drop_target_registration() {
        let result = register_file_drop_target(0, |_event| {});
        assert!(matches!(result, Err(OnyxError::Platform { .. })));
    }

    #[test]
    fn rejects_empty_path_list_for_hdrop_and_drag() {
        assert!(matches!(
            encode_cf_hdrop(&[]),
            Err(OnyxError::InvalidPath { .. })
        ));
        assert!(matches!(
            start_external_file_drag(&[]),
            Err(OnyxError::InvalidPath { .. })
        ));
    }

    #[test]
    fn cf_hdrop_roundtrip_unicode_and_long_paths() {
        let temp = TempDir::new().unwrap();
        let unicode_dir = temp.path().join("Ümlaut_日本語_🚀");
        let deep_segment = "very_long_directory_segment_0123456789".repeat(5);
        let long_path = unicode_dir.join(deep_segment).join("document.rs");
        let second_path = temp.path().join("spaces in name.txt");

        let original = vec![long_path, second_path];
        let encoded = encode_cf_hdrop(&original).expect("CF_HDROP encode should succeed");
        let decoded = decode_cf_hdrop(&encoded).expect("CF_HDROP decode should succeed");

        let expected: Vec<PathBuf> = original
            .iter()
            .map(|p| PathBuf::from(normalize_display_path(p)))
            .collect();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn data_object_serves_valid_cf_hdrop_medium() {
        let temp = TempDir::new().unwrap();
        let file_a = temp.path().join("alpha.txt");
        let file_b = temp.path().join("beta_Ω.txt");
        let paths = vec![file_a, file_b];

        let payload = encode_cf_hdrop(&paths).unwrap();
        let data_object: IDataObject = FileDataObject {
            hdrop_payload: payload,
        }
        .into();

        let fmt = hdrop_formatetc();
        // SAFETY: `fmt` is a valid `FORMATETC` pointer.
        let query_hr = unsafe { data_object.QueryGetData(&raw const fmt) };
        assert_eq!(query_hr, S_OK);

        // SAFETY: `fmt` is a valid `FORMATETC` pointer; returned `STGMEDIUM` is freed via `ReleaseStgMedium`.
        let mut medium = unsafe { data_object.GetData(&raw const fmt) }.unwrap();
        // SAFETY: `medium` has `tymed == TYMED_HGLOBAL`, so accessing `u.hGlobal` and locking it is sound.
        let decoded = unsafe {
            let hglobal = medium.u.hGlobal;
            let size = GlobalSize(hglobal);
            let ptr = GlobalLock(hglobal).cast::<u8>();
            assert!(!ptr.is_null());
            let slice = std::slice::from_raw_parts(ptr, size);
            let paths = decode_cf_hdrop(slice).unwrap();
            let _ = GlobalUnlock(hglobal);
            ReleaseStgMedium(&raw mut medium);
            paths
        };

        let expected: Vec<PathBuf> = paths
            .iter()
            .map(|p| PathBuf::from(normalize_display_path(p)))
            .collect();
        assert_eq!(decoded, expected);
    }
}
