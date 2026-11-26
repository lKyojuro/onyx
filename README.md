# ONYX

**Built with Rust. Designed for Windows.**

Onyx is a native, high-performance, privacy-respecting file explorer for Windows 11. It is built from the ground up with Rust, GPUI (`gpui-kit` + `gpui-fast` on DirectX 11 / DirectWrite), and native Win32/COM APIs — with zero Electron, Chromium, WebView, telemetry, or account requirements.

## Core Capabilities

- **Native Windows 11 UI**: DirectX 11 GPU-accelerated GPUI interface (`gpui-fast`) with obsidian dark & light themes, virtualized lists, and native Windows 11 shell & title-bar integration.
- **Non-Blocking Filesystem Engine**: Asynchronous directory enumeration with generation tokens, cancellation tokens, incremental updates, Rayon parallel metadata sorting, natural sorting, UNC & long-path (`\\?\`) normalization, hidden/system/symlink inspection, and live filesystem change notifications.
- **Safe File Operations Engine**: Copy, move, rename, mkdir, duplicate, Windows Recycle Bin deletion (default), opt-in permanent delete, progress reporting, cancellation, conflict resolution, disk space checks, recursion guards, and undo journal.
- **Multi-Tab & Dual-Pane Navigation**: Multi-tab bar, back/forward history, editable address bar with breadcrumbs, favorites/pins, known folders, drive enumeration with capacity bars, dual-pane split view, details & grid views, command palette (`Ctrl+K`), quick preview (`Space`), and Windows Terminal integration.
- **Dual-Mode Search Engine**: Streaming async filesystem search (filename, extension, size, modified date) plus SQLite FTS5 persistent search index with incremental reconciliation and NTFS USN Journal detection.
- **Safe Preview Engine**: On-demand previews for images, plain text, source code, Markdown, and binary/media/PDF metadata with strict size/memory/timeout limits and zero script execution.

## Workspace Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), [docs/ROADMAP.md](docs/ROADMAP.md), and [docs/PERFORMANCE.md](docs/PERFORMANCE.md).

## Building & Testing

Requires Rust 1.95+ (`x86_64-pc-windows-msvc`) and Windows 11 SDK / MSVC Build Tools.

```powershell
cargo fmt --all -- --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release
```

Run Onyx:

```powershell
cargo run --release --bin onyx
```
