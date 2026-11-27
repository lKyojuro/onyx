# Onyx Development Roadmap

## Phase 0 — Foundation
- [x] Repository & Cargo workspace initialization
- [x] Architecture & ADR documentation
- [x] CI pipeline configuration
- [x] Native Windows 11 Slint application window with design tokens

## Phase 1 — Functional Explorer
- [x] Asynchronous directory navigation with generation tokens
- [x] Sidebar (Home, Favorites, Known Folders, Drives with free-space indicators)
- [x] Editable address bar with breadcrumb segments
- [x] Virtualized file list with natural sorting and column headers
- [x] File metadata formatting and native Windows file icons
- [x] Onyx Dark Graphite and Light themes + system accent integration

## Phase 2 — Productivity
- [x] Multi-tab navigation & tab switching/closing/reordering
- [x] Back/forward navigation history
- [x] Favorites & pinned directories
- [x] Keyboard shortcuts (`Ctrl+T`, `Ctrl+W`, `Ctrl+L`, `Ctrl+F`, `Ctrl+K`, `F2`, `F5`, `Alt+Left/Right/Up`, `Space`, `Delete`, `Ctrl+C/X/V/Z`)
- [x] Copy, move, rename, new folder, duplicate, Windows Recycle Bin deletion
- [x] Live operation progress and cancellation
- [x] Windows clipboard file list integration & undo journal

## Phase 3 — Advanced Functionality
- [x] Dual-pane split mode
- [x] On-demand file preview pane & Spacebar Quick Look
- [x] Command palette (`Ctrl+K`)
- [x] Windows Terminal integration
- [x] Crash-safe session restoration
- [x] Asynchronous streaming background search with filters

## Phase 4 — Native Integration
- [x] Native Windows Properties dialog (`ShellExecuteExW` "properties")
- [x] Native file associations & Open With
- [x] Native Windows 11 DWM Mica / dark titlebar integration
- [x] Native Windows shell icons & known folders (`SHGetKnownFolderPath`)
- [x] COM STA guards & graceful fallbacks
- [x] System theme & accent color registry detection

## Phase 5 — Performance
- [x] Virtualized Slint `ListView` models for large directories
- [x] Bounded LRU caches for snapshots & icons
- [x] SQLite FTS5 persistent search index & incremental updater
- [x] NTFS USN Journal capability probe & graceful fallback
- [x] Reproducible Criterion benchmarks (`benchmarks/fs_benchmarks.rs`)

## Phase 6 — Production Readiness
- [x] Fault-injection & edge-case tests in isolated temporary directories
- [x] Crash-safe atomic config & session recovery
- [x] Release profile optimization (`LTO = "fat"`, `opt-level = "s"`, symbol stripping)
- [x] Packaging & distribution documentation (Portable ZIP, WiX MSI, MSIX, GitHub Release workflow)

## Phase 7 — v0.2 Power-User & Native Polish
- [x] Native Settings overlay (`Ctrl+,`) with live apply & atomic TOML persistence
- [x] Incremental SQLite FTS5 search index auto-updater (`notify` watcher + debounced reconciliation)
- [x] Safe ZIP archive engine (Create, Extract Here/To Folder, Browse overlay, Zip-Slip & Zip-Bomb guards)
- [x] Native Windows Drag & Drop (inbound `WM_DROPFILES`, outbound OLE `DoDragDrop`, compact internal drag preview)
- [x] Async LRU image thumbnails in Grid view (with OneDrive cloud-placeholder hydration protection)
- [x] Native Windows Shell icons for known folders and drives in the sidebar
- [x] Onyx logo-derived color palette & responsive 960px toolbar / modal mutual-exclusion polish
- [x] Asynchronous folder size calculation with LRU cache and proportional size bars in Details view
- [x] `%ENV%` variable expansion (`%APPDATA%`, `%USERPROFILE%`, `%TEMP%`, …) and live address bar path autocomplete

## Phase 8 — Path to v1.0 (Active)
- [x] Primary UI cutover to GPUI (`onyx-ui-gpui` with `gpui-kit` + `gpui-fast` on DirectX 11, ADR 0004; idle RAM reduced by ~75% to ~67 MB)
- [x] Recycle Bin shell namespace view with stock icons and native app icon (`onyx-windows` + `onyx-ui-gpui`)
- [ ] Inline filter & "Group By" view mode (core engine implemented in `onyx-core`; UI wiring & flattened subtree view pending)
- [ ] Inline collapsible Tree View in the file list (nested folder expansion with chevrons)
- [ ] Multi-selection polish (marquee/rubber-band selection, `Ctrl+Click` & `Shift+Click` range selection, inline rename)
- [ ] Resizable & reorderable metadata columns with per-folder view inheritance
- [ ] Live USB/removable drive hotplug & network share detection
- [ ] Extended archive formats (`.tar.gz`/`.tgz` implemented in `onyx-operations`; `.7z` pending)
- [ ] Idle memory footprint optimization (stretch target `< 50 MB` working set)
- [ ] Near-instant startup (post-Frame-1 deferred init, `/DELAYLOAD`, optional single-instance warm background tray mode `< 40 ms`)
- [ ] Low-overhead fast folder-size scanner (Win32 `FindFirstFileExW`/`NtQueryDirectoryFileEx`, `THREAD_MODE_BACKGROUND_BEGIN` low CPU/I/O priority, bottom-up subtree caching, optional NTFS MFT + USN live sizes)
- [ ] High-speed file transfer engine (SSD-aware parallel copy for small files, `CopyFile2` / overlapped unbuffered I/O + pre-allocation for large files, ReFS block cloning & SMB server-side copy)
- [ ] Full NTFS USN Journal real-time indexing integration
- [ ] Code-signed MSI/MSIX installers and official GitHub `v0.2.0` / `v1.0.0` release
