# Onyx System Architecture

## Overview

Onyx follows a layered, dependency-inverted Rust workspace architecture where core engines are completely decoupled from the GPUI presentation layer and Win32 platform APIs.

```text
onyx-ui-gpui (binary: onyx.exe — GPUI + gpui-fast DirectX 11 UI & Tokio runtime bridge)
 ├── onyx-core        (Domain Model: FileEntry, NavHistory, TabState, NaturalSort, Filter/Group, Errors)
 ├── onyx-config      (Persistence: atomic TOML settings, favorites, session restore)
 ├── onyx-cache       (Bounded LRU caches + SQLite metadata/icon/index store)
 ├── onyx-windows     (Platform Layer: Win32, COM, Shell, Recycle Bin, DWM, Icons, Drives, DnD, Clipboard)
 ├── onyx-fs          (Filesystem Engine: async enumeration, cancellation, watchers, folder sizes, long paths)
 ├── onyx-operations  (File Operations: copy, move, rename, recycle, undo journal, conflicts, ZIP/TGZ archives)
 ├── onyx-search      (Search Engine: Phase A streaming walk + Phase B SQLite FTS5 & USN)
 └── onyx-preview     (Preview Engine: bounded text/code/markdown/image/metadata previews & thumbnails)
```

## Threading Model

- **Main / UI Thread**: Exclusively runs the GPUI event loop (`gpui-kit` + `gpui-fast` on DirectX 11 / DirectWrite). Never performs disk I/O, directory walks, or blocking COM operations.
- **Tokio Async Runtime**: Coordinates navigation requests, cancellation tokens, debounced directory watchers, search, preview decoding, and file operation task dispatch.
- **Blocking / Rayon Worker Pools**: Executes directory enumeration (`ReadDir`), parallel sorting, image decoding, SQLite queries, and STA-initialized COM Shell calls (`IFileOperation`, `SHGetFileInfoW`, `ShellExecuteExW`, `DoDragDrop`).
- **UI State Synchronization**: Worker tasks deliver snapshots and incremental updates via GPUI async tasks and `Entity::update` guarded by per-pane `GenerationId` checks so superseded results are never applied.

## Data Integrity & Safety Boundaries

1. All `unsafe` Win32/COM interop is isolated inside `onyx-windows` and audited with `// SAFETY:` invariants and RAII guards (`ComGuard`, `HIconGuard`, `HBitmapGuard`).
2. `onyx-operations` prevents recursive self-copy/self-move (copying a directory into its own descendant), validates free disk space where queryable, respects conflict policies (`Ask`, `Skip`, `Overwrite`, `Rename`), and routes standard deletion exclusively to the Windows Recycle Bin.
3. `onyx-config` persists configuration and session state using temp-file + flush + atomic replace to prevent corruption on unexpected power loss or crash.
4. `onyx-preview` treats every file as untrusted input with strict byte limits, decode timeouts, and zero active content execution.
