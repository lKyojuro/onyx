# `onyx-ui-gpui` — Onyx GPUI (`gpui-fast`) Main UI

Primary Windows 11 desktop frontend for Onyx built on [`gpui-kit`](https://crates.io/crates/gpui-kit) `0.11.0` with `features = ["gpui-fast"]` (DirectX 11 + DirectWrite), wired directly to all 8 UI-free core crates and producing the main `onyx.exe` binary.

## Highlights

- **Workspace Member**: Integrated into the root Cargo workspace (`[[bin]] name = "onyx"`), owning the workspace integration tests (`tests/integration_tests.rs`) and Criterion benchmarks (`benchmarks/fs_benchmarks.rs`).
- **Direct Core Integration**: Uses `onyx-fs::FsEngine` and `onyx-core` (`GenerationId`, `NavHistory`, `FileEntry`, `build_breadcrumbs`, `format_bytes`, `format_timestamp`) with cancellation-aware async directory enumeration on a bounded Tokio/Rayon pool (never blocking the GPUI UI thread).
- **Virtualized List & Navigation**: Renders entries via `gpui_kit::uniform_list` with Back / Forward / Up / Refresh controls, clickable breadcrumbs, sidebar quick-links, single-click selection, and double-click folder entry.
- **Obsidian & LED Dot-Matrix Design**: Deep matte obsidian surfaces (`#080808`–`#141414`), subtle `#1e1e1e` borders, and a 7×7 warm-amber LED dot-matrix status pill (`src/led_pill.rs`) with animated scanning wave + typography shimmer while active and zero timer wakeups at idle.
- **Command Palette & Settings**: `Ctrl+K` opens a searchable command palette (`src/components/command_palette.rs`) mirroring `crates/onyx-app/src/commands.rs`, `Ctrl+,` opens the settings overlay (`src/components/settings_overlay.rs`) for theme, default view, hidden/system files, preview, dual-pane default, folder sizes, terminal profile, indexed search and excluded index folders. Both stay modal, close on `Escape`, and persist through `ConfigStore` (atomic write-to-temp-and-rename).
- **Indexed Search (`onyx-search`)**: `src/search_index.rs` owns the persistent SQLite FTS5 index at `cache/search.sqlite` on a dedicated worker thread (open, debounced `notify` watcher, `CancellationToken` shutdown), so hits survive a restart and the UI thread never waits for SQLite. Every search run gets its own `GenerationId` plus `CancellationToken`; stale or superseded results are dropped and accepted ones are delivered through `Entity::update`. The `search-index-chip`, the `index` / `index-rebuild` palette commands and the `indexed_search` setting all drive the same lifecycle, and a query prefixed with `?` in the palette lists file hits across the folder tree.
- **CLI Parity**: Supports `--version` / `-V`, `--smoke-test`, and optional `[PATH]` for direct benchmarking with `scripts/measure-perf.ps1`.

## Windows Build Caveats (`gpui-kit` + `gpui-fast`)

1. **Release HLSL Shader Precompilation (`fxc.exe`)**: In `--release` builds, `gpui-fast-windows` and `gpui-pre-windows` run `fxc.exe` in `build.rs` to precompile `shaders.hlsl` (`#include "alpha_correction.hlsl"`). This requires the Windows SDK shader compiler (`fxc.exe`) and filesystem read access to the Cargo registry source directory. Dev/test builds skip build-time `fxc.exe`.
2. **Additive Cargo Features in `gpui-kit 0.11.0`**: Enabling `features = ["gpui-fast"]` switches the active backend to `gpui-fast 0.1.19` cleanly without import changes, but Cargo still compiles `gpui-pre` / `gpui-pre-windows` in the dependency graph. Release LTO + symbol stripping eliminates unused code (`24.09 MB` final EXE).

## Benchmark Comparison (`scripts/measure-perf.ps1`)

Measured on Windows 11 Home (`10.0.26300`), AMD Ryzen 7 5700X3D (8C/16T), 31.93 GB RAM.  
Release profile: `opt-level = "s"`, `lto = "thin"`, `codegen-units = 16`, `strip = "symbols"`.

| Metric | Slint `release-fast` Baseline | `onyx-ui-gpui` (`gpui-fast`) | Delta |
|---|---:|---:|---:|
| **EXE Size** | `21.23 MB` | **`24.09 MB`** (`25,265,152` B) | **+2.86 MB (+13.5%)** |
| **Window-Ready** | `142.99 ms` | **`232.88 ms`** | **+89.89 ms (+62.9%)** |
| **3s Idle WorkingSet64** | `269.43 MB` | **`67.37 MB`** (`70,643,712` B) | **-202.06 MB (-75.0%)** |
| **3s Idle PrivateMemory64** | `455.86 MB` | **`66.88 MB`** (`70,127,616` B) | **-388.98 MB (-85.3%)** |
| **`--smoke-test` Median (5 runs)** | — | `1016.77 ms` (`1005.93–1021.22 ms`) | — |

## Commands

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release --bin onyx
.\scripts\measure-perf.ps1 -BinaryPath target\release\onyx.exe -Runs 5 -IdleSeconds 3
```
