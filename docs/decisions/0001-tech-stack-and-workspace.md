# ADR 0001: Technology Stack and Cargo Workspace Layout

## Status
Accepted

## Context
Onyx requires a native Windows 11 file explorer with sub-300ms startup, low idle RAM, GPU-accelerated Fluent UI, strong data-integrity guarantees, and clean modularity without WebView/Electron overhead.

## Decision
- **Language**: Rust (2024 edition, stable `x86_64-pc-windows-msvc`).
- **UI Toolkit**: `slint` 1.19 with default FemtoVG GPU renderer (automatic software fallback), AccessKit accessibility support, and `raw-window-handle-06` for native HWND access.
- **Platform APIs**: `windows` 0.62 (`windows-rs`) isolated strictly inside `onyx-windows`.
- **Concurrency**: `tokio` for async task orchestration & cancellation; `rayon` for parallel sorting & CPU-heavy batch operations.
- **Persistence**: `rusqlite` (bundled SQLite with FTS5) for search index & metadata cache; `toml` + `serde` with atomic file writes for user configuration and session recovery.
- **Workspace**: 10 focused crates (`onyx-core`, `onyx-config`, `onyx-cache`, `onyx-windows`, `onyx-fs`, `onyx-operations`, `onyx-search`, `onyx-preview`, `onyx-ui`, `onyx-app`) enforcing a strict acyclic dependency graph.

## Consequences
- Fast incremental compilation via crate separation.
- Core logic and engines can be unit- and integration-tested headlessly without starting a GUI window.
- Release binary stays compact with zero external DLL dependencies beyond standard Windows system DLLs.
