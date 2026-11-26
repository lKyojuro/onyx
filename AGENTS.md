# AGENTS.md — Onyx Engineering Guidelines

## Architecture Rules

1. **No UI dependencies in core**: `onyx-core`, `onyx-fs`, `onyx-operations`, `onyx-search`, `onyx-preview`, `onyx-cache`, `onyx-config`, and `onyx-windows` must never depend on `onyx-ui-gpui` or `gpui-kit`.
2. **Isolate platform code**: All `unsafe` Win32/COM/Shell calls live strictly inside `onyx-windows` with clear `// SAFETY:` comments and safe Rust wrappers.
3. **Never block the UI thread**: All filesystem I/O, search, preview loading, and file operations run on Tokio/blocking worker pools and deliver results to the GPUI event loop via async tasks and `Entity::update`.
4. **Generation tracking**: Directory navigation and search requests carry monotonically increasing `GenerationId` tokens and `CancellationToken`s so stale async results are dropped cleanly.
5. **Data integrity first**:
   - Default delete ALWAYS uses Windows Recycle Bin (`IFileOperation` / `SHFileOperationW`).
   - Permanent deletion is explicit opt-in only.
   - All file operation tests run strictly inside isolated `tempfile::TempDir` directories.
   - Config and session persistence use atomic write-to-temp-and-rename.

## Quality Gates

Before completing any milestone:

```powershell
cargo fmt --all -- --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release
```
