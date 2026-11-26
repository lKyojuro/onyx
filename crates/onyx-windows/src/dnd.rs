//! Native Windows external file Drag & Drop (`WM_DROPFILES` inbound + OLE `DoDragDrop` outbound).

use crate::icons::{RgbaIcon, extract_path_icon_rgba};
use crate::own_window;
use onyx_core::{OnyxError, OnyxResult, normalize_display_path};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString, c_void};
use std::marker::PhantomData;
use std::mem::{ManuallyDrop, size_of};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{
    COLORREF, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, DV_E_DVASPECT,
    DV_E_FORMATETC, DV_E_TYMED, E_FAIL, E_NOTIMPL, E_POINTER, FALSE, GlobalFree, HGLOBAL, HWND,
    LPARAM, LRESULT, OLE_E_ADVISENOTSUPPORTED, POINT, RPC_E_CHANGED_MODE, S_FALSE, S_OK, SIZE,
    TRUE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDIBSection, DIB_RGB_COLORS, DeleteObject, HBITMAP,
    HGDIOBJ,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink,
    IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, IStream, STATFLAG_NONAME,
    STATSTG, STGMEDIUM, STGMEDIUM_0, STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE, DoDragDrop,
    IDropSource, IDropSource_Impl, OleInitialize, OleUninitialize, ReleaseStgMedium,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MK_RBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, ReleaseCapture, VK_LBUTTON};
use windows::Win32::UI::Shell::{
    CLSID_DragDropHelper, DROPFILES, DefSubclassProc, DragAcceptFiles, DragFinish, DragQueryFileW,
    DragQueryPoint, HDROP, IDragSourceHelper, RemoveWindowSubclass, SHCreateMemStream,
    SHCreateStdEnumFmtEtc, SHDRAGIMAGE, SetWindowSubclass,
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

/// Window handle and client-area point an external drag starts from, used to let the shell
/// snapshot a drag image.
///
/// Windows copies the window content under `(x, y)` into a translucent bitmap that follows the
/// pointer, which is what makes the held item look like it is carried along instead of only
/// showing the default drag cursor. Pass `None` to [`start_external_file_drag`] to keep the
/// plain cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DragImageSource {
    /// Handle of the window that renders the dragged item.
    pub hwnd: isize,
    /// Client-area X coordinate of the drag start point.
    pub x: i32,
    /// Client-area Y coordinate of the drag start point.
    pub y: i32,
}

impl DragImageSource {
    /// Builds a drag image source from a raw window handle and a client-area point.
    #[must_use]
    pub const fn new(hwnd: isize, x: i32, y: i32) -> Self {
        Self { hwnd, x, y }
    }
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
    hglobal_formatetc(CF_HDROP.0)
}

fn hglobal_formatetc(cf_format: u16) -> FORMATETC {
    FORMATETC {
        cfFormat: cf_format,
        ptd: ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// Clipboard format the shell's drag-drop helper stores the generated drag image under
/// (`CFSTR_DRAGIMAGEBITS`), registered once per process.
#[cfg(test)]
fn drag_image_format() -> u16 {
    static FORMAT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();
    *FORMAT.get_or_init(|| {
        // SAFETY: The literal is a valid null-terminated wide string; registering a clipboard
        // format only returns its identifier and never touches the object being dragged.
        let id = unsafe {
            windows::Win32::System::DataExchange::RegisterClipboardFormatW(windows::core::w!(
                "DragImageBits"
            ))
        };
        u16::try_from(id).unwrap_or(0)
    })
}

fn matches_hdrop_format(fmt: &FORMATETC) -> Result<(), HRESULT> {
    matches_hglobal_format(fmt, CF_HDROP.0)
}

fn matches_hglobal_format(fmt: &FORMATETC, cf_format: u16) -> Result<(), HRESULT> {
    if fmt.cfFormat != cf_format {
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

#[derive(Clone)]
enum StoredMedium {
    HGlobal(Vec<u8>),
    Stream(Vec<u8>),
}

impl StoredMedium {
    fn tymed(&self) -> u32 {
        match self {
            Self::HGlobal(_) => TYMED_HGLOBAL.0 as u32,
            Self::Stream(_) => TYMED_ISTREAM.0 as u32,
        }
    }
}

/// Reads the full byte contents of a COM `IStream` without consuming its ownership.
fn read_istream_bytes(stream: &IStream) -> Option<Vec<u8>> {
    // SAFETY: `stream` is a live COM `IStream` passed to `SetData`.
    unsafe {
        let mut stat = STATSTG::default();
        let _ = stream.Stat(&raw mut stat, STATFLAG_NONAME);
        let _ = stream.Seek(0, STREAM_SEEK_SET, None);
        let cap = usize::try_from(stat.cbSize)
            .unwrap_or(0)
            .min(16 * 1024 * 1024);
        let mut out = Vec::with_capacity(cap);
        let mut chunk = [0u8; 4096];
        loop {
            let mut read_bytes = 0u32;
            let hr = stream.Read(
                chunk.as_mut_ptr().cast::<c_void>(),
                chunk.len() as u32,
                Some(&raw mut read_bytes),
            );
            if read_bytes > 0 {
                out.extend_from_slice(&chunk[..read_bytes as usize]);
            }
            if hr.is_err() || read_bytes == 0 || out.len() >= 16 * 1024 * 1024 {
                break;
            }
        }
        Some(out)
    }
}

#[implement(IDataObject)]
struct FileDataObject {
    hdrop_payload: Vec<u8>,
    /// Auxiliary formats written by the shell's `IDragSourceHelper`
    /// (`DragImageBits`, `DragContext`, `IsShowingLayered`, `UsingDefaultDragImage`, etc.),
    /// served back through `GetData` so Windows Explorer and other drop targets paint the
    /// translucent drag icon under the cursor.
    extra_formats: Mutex<HashMap<u16, StoredMedium>>,
}

impl FileDataObject {
    fn new(hdrop_payload: Vec<u8>) -> Self {
        Self {
            hdrop_payload,
            extra_formats: Mutex::new(HashMap::new()),
        }
    }

    fn extra_format(&self, cf_format: u16) -> Option<StoredMedium> {
        self.extra_formats
            .lock()
            .ok()
            .and_then(|map| map.get(&cf_format).cloned())
    }

    fn extra_format_entries(&self) -> Vec<(u16, u32)> {
        self.extra_formats
            .lock()
            .ok()
            .map(|map| map.iter().map(|(&k, v)| (k, v.tymed())).collect())
            .unwrap_or_default()
    }
}

/// Copies the bytes of a `TYMED_HGLOBAL` medium out of global memory without taking ownership.
fn read_hglobal_bytes(hglobal: HGLOBAL) -> Option<Vec<u8>> {
    // SAFETY: `hglobal` is a global memory handle owned by the caller of `SetData`.
    unsafe {
        let size = GlobalSize(hglobal);
        if size == 0 {
            return None;
        }
        let locked = GlobalLock(hglobal).cast::<u8>();
        if locked.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(locked, size).to_vec();
        let _ = GlobalUnlock(hglobal);
        Some(bytes)
    }
}

impl IDataObject_Impl for FileDataObject_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        if pformatetcin.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: `pformatetcin` was verified non-null above.
        let fmt = unsafe { &*pformatetcin };
        if fmt.cfFormat == CF_HDROP.0 {
            if let Err(hr) = matches_hdrop_format(fmt) {
                return Err(hr.into());
            }
            let hglobal = alloc_hglobal_with_bytes(&self.hdrop_payload)?;
            return Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 { hGlobal: hglobal },
                pUnkForRelease: ManuallyDrop::new(None),
            });
        }

        let Some(stored) = self.extra_format(fmt.cfFormat) else {
            return Err(DV_E_FORMATETC.into());
        };
        if (fmt.dwAspect & DVASPECT_CONTENT.0) == 0 {
            return Err(DV_E_DVASPECT.into());
        }
        if (fmt.tymed & stored.tymed()) == 0 {
            return Err(DV_E_TYMED.into());
        }

        match stored {
            StoredMedium::HGlobal(bits) => {
                let hglobal = alloc_hglobal_with_bytes(&bits)?;
                Ok(STGMEDIUM {
                    tymed: TYMED_HGLOBAL.0 as u32,
                    u: STGMEDIUM_0 { hGlobal: hglobal },
                    pUnkForRelease: ManuallyDrop::new(None),
                })
            }
            StoredMedium::Stream(bits) => {
                // SAFETY: `SHCreateMemStream` copies `bits` into a newly allocated COM `IStream`.
                let stream = unsafe { SHCreateMemStream(Some(&bits)) }.ok_or(E_FAIL)?;
                Ok(STGMEDIUM {
                    tymed: TYMED_ISTREAM.0 as u32,
                    u: STGMEDIUM_0 {
                        pstm: ManuallyDrop::new(Some(stream)),
                    },
                    pUnkForRelease: ManuallyDrop::new(None),
                })
            }
        }
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
        if fmt.cfFormat == CF_HDROP.0 {
            return match matches_hdrop_format(fmt) {
                Ok(()) => S_OK,
                Err(hr) => hr,
            };
        }
        if let Some(stored) = self.extra_format(fmt.cfFormat) {
            if (fmt.dwAspect & DVASPECT_CONTENT.0) == 0 {
                return DV_E_DVASPECT;
            }
            if (fmt.tymed & stored.tymed()) == 0 {
                return DV_E_TYMED;
            }
            return S_OK;
        }
        DV_E_FORMATETC
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
        pformatetc: *const FORMATETC,
        pmedium: *const STGMEDIUM,
        frelease: BOOL,
    ) -> windows::core::Result<()> {
        if pformatetc.is_null() || pmedium.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: Both pointers were verified non-null above and stay valid for the call.
        let (fmt, medium) = unsafe { (&*pformatetc, &*pmedium) };
        // SAFETY: `medium.tymed` is checked before reading the matching union field.
        let stored = unsafe {
            if fmt.cfFormat == 0 {
                None
            } else if medium.tymed == TYMED_HGLOBAL.0 as u32 && !medium.u.hGlobal.is_invalid() {
                read_hglobal_bytes(medium.u.hGlobal).map(StoredMedium::HGlobal)
            } else if medium.tymed == TYMED_ISTREAM.0 as u32 {
                medium
                    .u
                    .pstm
                    .as_ref()
                    .and_then(read_istream_bytes)
                    .map(StoredMedium::Stream)
            } else {
                None
            }
        };
        let Some(stored) = stored else {
            // Per COM `IDataObject::SetData` contract, when returning an error the caller retains
            // ownership of `pmedium` and will release it itself — never call `ReleaseStgMedium`
            // on an error path or the caller will double-free (`STATUS_HEAP_CORRUPTION`).
            return Err(E_NOTIMPL.into());
        };
        match self.extra_formats.lock() {
            Ok(mut map) => {
                map.insert(fmt.cfFormat, stored);
            }
            Err(_) => return Err(E_FAIL.into()),
        }
        if frelease.as_bool() {
            // SAFETY: `SetData` succeeded with `frelease == TRUE`, so ownership of `pmedium` was
            // transferred to us and we release it once now that the bytes have been copied.
            unsafe {
                let mut owned = ptr::read(pmedium);
                ReleaseStgMedium(&raw mut owned);
            }
        }
        Ok(())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        if dwdirection != (DATADIR_GET.0 as u32) {
            return Err(E_NOTIMPL.into());
        }
        let mut formats = vec![hdrop_formatetc()];
        for (cf_format, tymed) in self.extra_format_entries() {
            if cf_format != CF_HDROP.0 {
                formats.push(FORMATETC {
                    cfFormat: cf_format,
                    ptd: ptr::null_mut(),
                    dwAspect: DVASPECT_CONTENT.0,
                    lindex: -1,
                    tymed,
                });
            }
        }
        // SAFETY: `formats` is a valid slice of `FORMATETC` structs copied by `SHCreateStdEnumFmtEtc`.
        unsafe { SHCreateStdEnumFmtEtc(&formats) }
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

/// Creates a 32-bit top-down premultiplied BGRA DIB section from a straight-alpha [`RgbaIcon`].
fn create_premultiplied_bgra_dib(icon: &RgbaIcon) -> OnyxResult<HBITMAP> {
    let width = i32::try_from(icon.width).map_err(|_| OnyxError::Platform {
        message: "drag icon width exceeds i32".to_string(),
    })?;
    let height = i32::try_from(icon.height).map_err(|_| OnyxError::Platform {
        message: "drag icon height exceeds i32".to_string(),
    })?;
    let expected = (icon.width as usize)
        .checked_mul(icon.height as usize)
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| OnyxError::Platform {
            message: "drag icon dimensions overflow".to_string(),
        })?;
    if icon.width == 0 || icon.height == 0 || icon.pixels.len() != expected {
        return Err(OnyxError::Platform {
            message: "drag icon pixel buffer has invalid dimensions".to_string(),
        });
    }

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // Negative height creates a top-down 32-bit DIB matching `RgbaIcon` row order.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: [Default::default()],
    };

    let mut bits_ptr: *mut c_void = ptr::null_mut();
    // SAFETY: `bmi` describes a valid 32-bit top-down uncompressed DIB; `CreateDIBSection` writes
    // the backing pixel buffer pointer into `bits_ptr` and returns an owned `HBITMAP`.
    let hbitmap = unsafe {
        CreateDIBSection(
            None,
            &raw const bmi,
            DIB_RGB_COLORS,
            &raw mut bits_ptr,
            None,
            0,
        )
    }
    .map_err(|error| OnyxError::Platform {
        message: format!("CreateDIBSection failed for drag icon: {error}"),
    })?;

    if bits_ptr.is_null() {
        // SAFETY: `hbitmap` was just created and is unused on error.
        let _ = unsafe { DeleteObject(HGDIOBJ(hbitmap.0)) };
        return Err(OnyxError::Platform {
            message: "CreateDIBSection returned a null pixel pointer".to_string(),
        });
    }

    // SAFETY: `bits_ptr` points to `expected` writable bytes allocated by `CreateDIBSection`.
    let dst = unsafe { std::slice::from_raw_parts_mut(bits_ptr.cast::<u8>(), expected) };
    for (src_px, dst_px) in icon.pixels.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        let a = u16::from(src_px[3]);
        let r = ((u16::from(src_px[0]) * a + 127) / 255) as u8;
        let g = ((u16::from(src_px[1]) * a + 127) / 255) as u8;
        let b = ((u16::from(src_px[2]) * a + 127) / 255) as u8;
        dst_px[0] = b;
        dst_px[1] = g;
        dst_px[2] = r;
        dst_px[3] = src_px[3];
    }

    Ok(hbitmap)
}

/// Attaches the Windows Shell icon of `path` as a translucent cursor-following drag image via
/// `IDragSourceHelper::InitializeFromBitmap`.
///
/// Unlike `InitializeFromWindow`, this works for DirectX / custom-rendered windows (GPUI) that do
/// not handle the Win32 `DI_GETDRAGIMAGE` message.
fn attach_icon_drag_image(path: &Path, data_object: &IDataObject) -> OnyxResult<()> {
    let icon = extract_path_icon_rgba(path, path.is_dir()).ok_or_else(|| OnyxError::Platform {
        message: format!("could not extract shell icon for {}", path.display()),
    })?;
    let hbitmap = create_premultiplied_bgra_dib(&icon)?;

    // SAFETY: `CLSID_DragDropHelper` is an in-process COM class created on the calling STA
    // thread, which the caller initialized via `OleGuard`.
    let helper: IDragSourceHelper =
        match unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) } {
            Ok(helper) => helper,
            Err(error) => {
                // SAFETY: `hbitmap` was not transferred to `IDragSourceHelper`, so free it here.
                let _ = unsafe { DeleteObject(HGDIOBJ(hbitmap.0)) };
                return Err(OnyxError::Platform {
                    message: format!("IDragSourceHelper is unavailable: {error}"),
                });
            }
        };

    let width = i32::try_from(icon.width).unwrap_or(32);
    let height = i32::try_from(icon.height).unwrap_or(32);
    let mut shdi = SHDRAGIMAGE {
        sizeDragImage: SIZE {
            cx: width,
            cy: height,
        },
        ptOffset: POINT {
            x: width / 2,
            y: height / 2,
        },
        hbmpDragImage: hbitmap,
        crColorKey: COLORREF(0xFFFF_FFFF),
    };

    // SAFETY: `shdi` points to a valid `SHDRAGIMAGE` with an owned 32-bit DIB, and `data_object`
    // is a valid COM `IDataObject`. On success `InitializeFromBitmap` takes ownership of
    // `hbmpDragImage`; on failure we delete `hbitmap` ourselves.
    let res = unsafe { helper.InitializeFromBitmap(&raw mut shdi, data_object) };
    if let Err(error) = res {
        let _ = unsafe { DeleteObject(HGDIOBJ(hbitmap.0)) };
        return Err(OnyxError::Platform {
            message: format!("InitializeFromBitmap failed: {error}"),
        });
    }
    Ok(())
}

/// Lets the shell's drag-drop helper snapshot the window area under `source` into the drag image
/// of `data_object`.
///
/// Must be called on the STA thread that already initialized OLE, before `DoDragDrop` runs.
fn attach_drag_image(source: DragImageSource, data_object: &IDataObject) -> OnyxResult<()> {
    let window = own_window(source.hwnd)?;
    // SAFETY: `CLSID_DragDropHelper` is an in-process COM class created on the calling STA
    // thread, which the caller initialized via `OleGuard`.
    let helper: IDragSourceHelper =
        unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) }.map_err(
            |error| OnyxError::Platform {
                message: format!("IDragSourceHelper is unavailable: {error}"),
            },
        )?;
    let point = POINT {
        x: source.x,
        y: source.y,
    };
    // SAFETY: `own_window` verified that `window` belongs to this process, `point` stays alive
    // for the whole call, and `data_object` is a valid COM interface the helper only reads from.
    unsafe { helper.InitializeFromWindow(Some(window), Some(&raw const point), data_object) }
        .map_err(|error| OnyxError::Platform {
            message: format!("drag image initialization failed: {error}"),
        })
}

/// Initiates an outbound synchronous Windows OLE drag-and-drop loop for `paths` using a
/// `CF_HDROP` [`IDataObject`] and [`IDropSource`].
///
/// `drag_image` optionally names the window and client-area point the drag starts from; Windows
/// then snapshots that area into a bitmap that follows the pointer. Without it the drag runs with
/// the default cursor only.
///
/// Must be called on an STA thread (typically the UI thread when a pointer drag threshold is crossed).
pub fn start_external_file_drag(
    paths: &[PathBuf],
    drag_image: Option<DragImageSource>,
) -> OnyxResult<DragOutcome> {
    let hdrop_payload = encode_cf_hdrop(paths)?;
    let _ole = OleGuard::init()?;
    // SAFETY: VK_LBUTTON is a valid virtual-key code and this call reads the current input state.
    if unsafe { GetAsyncKeyState(i32::from(VK_LBUTTON.0)) } >= 0 {
        return Ok(DragOutcome::Cancelled);
    }
    // SAFETY: The caller uses the window's STA thread. ReleaseCapture only releases mouse capture
    // owned by this thread, allowing OLE to take over the active drag.
    let _ = unsafe { ReleaseCapture() };

    let data_object: IDataObject = FileDataObject::new(hdrop_payload).into();
    // Prefer the real Windows Shell icon via `InitializeFromBitmap` (works natively on DirectX /
    // GPUI windows where `InitializeFromWindow` / `DI_GETDRAGIMAGE` is unhandled), falling back to
    // window snapshot only if icon extraction fails.
    if let Err(icon_err) = attach_icon_drag_image(&paths[0], &data_object) {
        tracing::debug!(%icon_err, "shell icon drag image failed, trying window snapshot");
        if let Some(source) = drag_image
            && let Err(error) = attach_drag_image(source, &data_object)
        {
            tracing::debug!(%error, "starting the external drag without a drag image");
        }
    }
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
            start_external_file_drag(&[], None),
            Err(OnyxError::InvalidPath { .. })
        ));
        assert!(matches!(
            start_external_file_drag(&[], Some(DragImageSource::new(0, 0, 0))),
            Err(OnyxError::InvalidPath { .. })
        ));
    }

    #[test]
    fn drag_image_rejects_a_foreign_window() {
        let payload = encode_cf_hdrop(&[PathBuf::from("C:\\temp\\dragged.txt")]).unwrap();
        let data_object: IDataObject = FileDataObject::new(payload).into();
        let _ole = OleGuard::init().expect("OLE should initialize on this test thread");

        let result = attach_drag_image(DragImageSource::new(0, 10, 20), &data_object);
        assert!(matches!(result, Err(OnyxError::Platform { .. })));
    }

    #[test]
    fn icon_drag_image_populates_data_object_formats() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("sample.txt");
        std::fs::write(&file, b"hello").unwrap();

        let payload = encode_cf_hdrop(std::slice::from_ref(&file)).unwrap();
        let data_object: IDataObject = FileDataObject::new(payload).into();
        let _ole = OleGuard::init().expect("OLE should initialize on this test thread");

        attach_icon_drag_image(&file, &data_object)
            .expect("InitializeFromBitmap should attach the shell icon drag image");

        let img_fmt = hglobal_formatetc(drag_image_format());
        // SAFETY: `img_fmt` is a valid `FORMATETC` pointer.
        assert_eq!(
            unsafe { data_object.QueryGetData(&raw const img_fmt) },
            S_OK
        );
    }

    #[test]
    fn data_object_stores_the_drag_image_bits() {
        let payload = encode_cf_hdrop(&[PathBuf::from("C:\\temp\\dragged.txt")]).unwrap();
        let data_object: IDataObject = FileDataObject::new(payload).into();
        let fmt = hglobal_formatetc(drag_image_format());
        // The helper only offers the image format after `SetData` stored one.
        // SAFETY: `fmt` is a valid `FORMATETC` pointer.
        assert_eq!(
            unsafe { data_object.QueryGetData(&raw const fmt) },
            DV_E_FORMATETC
        );

        let image_bits = vec![0x5A_u8; 64];
        let mut medium = STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 {
                hGlobal: alloc_hglobal_with_bytes(&image_bits).unwrap(),
            },
            pUnkForRelease: ManuallyDrop::new(None),
        };
        // SAFETY: `fmt` and `medium` are valid pointers, and `false` keeps their ownership here.
        unsafe { data_object.SetData(&raw const fmt, &raw const medium, false) }
            .expect("the drag image bits should be accepted");
        // SAFETY: `medium` was not transferred (`fRelease == FALSE`), so it is freed here.
        unsafe { ReleaseStgMedium(&raw mut medium) };

        // SAFETY: `fmt` is a valid `FORMATETC` pointer.
        assert_eq!(unsafe { data_object.QueryGetData(&raw const fmt) }, S_OK);

        // SAFETY: `fmt` is valid; the returned `STGMEDIUM` is freed via `ReleaseStgMedium` below.
        let mut served = unsafe { data_object.GetData(&raw const fmt) }.unwrap();
        // SAFETY: `served` has `tymed == TYMED_HGLOBAL`, so its `u.hGlobal` can be locked and read.
        let decoded = unsafe {
            let hglobal = served.u.hGlobal;
            let size = GlobalSize(hglobal);
            let ptr = GlobalLock(hglobal).cast::<u8>();
            assert!(!ptr.is_null());
            let slice = std::slice::from_raw_parts(ptr, size);
            let decoded = slice.to_vec();
            let _ = GlobalUnlock(hglobal);
            ReleaseStgMedium(&raw mut served);
            decoded
        };
        assert_eq!(decoded, image_bits);
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
        let data_object: IDataObject = FileDataObject::new(payload).into();

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
