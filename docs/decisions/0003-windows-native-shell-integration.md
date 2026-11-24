# ADR 0003: Windows Native Shell, COM, and DWM Integration

## Status
Accepted

## Context
Windows Explorer behaviors (Recycle Bin, Properties dialog, Known Folders, file icons, Mica backdrop, clipboard `CF_HDROP`, file associations) depend on Win32 and COM APIs with strict apartment threading rules.

## Decision
1. All `unsafe` Win32/COM code is encapsulated in `onyx-windows`.
2. COM operations (`IFileOperation`, `SHGetKnownFolderPath`, `SHGetFileInfoW`) run on dedicated blocking threads guarded by an RAII `ComGuard` that calls `CoInitializeEx(COINIT_APARTMENTTHREADED)` and pairs `CoUninitialize()` on drop (tolerating `RPC_E_CHANGED_MODE` safely).
3. Window chrome uses `DwmSetWindowAttribute` for `DWMWA_USE_IMMERSIVE_DARK_MODE` and `DWMWA_SYSTEMBACKDROP_TYPE` (Mica / Mica Alt) on Windows 11 builds >= 22621 with graceful no-op fallback on unsupported hosts.
4. Default deletion uses `IFileOperation` with `FOFX_RECYCLEONDELETE` / `FOF_ALLOWUNDO` so files safely go to the Windows Recycle Bin.
5. Onyx does not modify shell registration or replace `explorer.exe`.

## Consequences
- Correct COM apartment semantics without polluting the Tokio thread pool or Slint UI thread.
- Graceful degradation if an API is unavailable or returns an error.
