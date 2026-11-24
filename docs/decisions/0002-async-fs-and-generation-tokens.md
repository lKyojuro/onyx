# ADR 0002: Asynchronous Filesystem Engine and Generation Tokens

## Status
Accepted

## Context
Opening directories with 100,000+ files or slow SMB/UNC network shares can stall UI threads or cause race conditions where an earlier slow request overwrites a newer navigation target.

## Decision
1. Every navigation request is assigned a monotonically increasing `GenerationId` (`u64`) paired with a `tokio_util::sync::CancellationToken`.
2. Navigating to a new path immediately cancels the previous pane's `CancellationToken` and increments its `GenerationId`.
3. Enumeration runs on a blocking worker thread, streaming batches via bounded channels (`mpsc`) and checking cancellation every batch.
4. UI updates verify `event.generation == current_pane_generation` before applying any entries to the view model.
5. Paths longer than `MAX_PATH` (260 chars) are transparently normalized using extended-length prefixes (`\\?\` and `\\?\UNC\`) for I/O while preserving clean display paths for the UI.

## Consequences
- Rapid keyboard navigation (`Alt+Left`, `Backspace`, clicking breadcrumbs) never blocks the UI and never displays stale directory contents.
- Long paths and UNC shares work seamlessly without silent truncation.
