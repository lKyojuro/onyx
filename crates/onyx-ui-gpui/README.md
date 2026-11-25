# `onyx-ui-gpui` — GPUI + `gpui-fast` Next-Gen UI Prototype

Standalone Windows prototype exploring Onyx's next-generation UI built on [`gpui-kit`](https://crates.io/crates/gpui-kit) `0.11.0` with `features = ["gpui-fast"]` (`gpui-fast 0.1.19`), wired directly to `onyx-fs` and `onyx-core`.

## Highlights

- **Isolated Workspace**: Declares its own `[workspace]` table in `Cargo.toml` so the root workspace `Cargo.toml` and `Cargo.lock` remain untouched.
- **Direct Core Integration**: Uses `onyx-fs::FsEngine` and `onyx-core` (`GenerationId`, `NavHistory`, `FileEntry`, `build_breadcrumbs`, `format_bytes`, `format_timestamp`) with cancellation-aware async directory enumeration on a bounded Tokio/Rayon pool (never blocking the GPUI UI thread).
- **Virtualized List & Navigation**: Renders entries via `gpui_kit::uniform_list` with Back / Forward / Up / Refresh controls, clickable breadcrumbs, sidebar quick-links, single-click selection, and double-click folder entry.
- **Obsidian & LED Dot-Matrix Design**: Deep matte obsidian surfaces (`#080808`–`#141414`), subtle `#1e1e1e` borders, and a 7×7 warm-amber LED dot-matrix status pill (`src/led_pill.rs`) with animated scanning wave + typography shimmer while active and zero timer wakeups at idle.
- **CLI Parity**: Supports `--version` / `-V`, `--smoke-test`, and optional `[PATH]` for direct benchmarking with `scripts/measure-perf.ps1`.

## Windows Build Caveats (`gpui-kit` + `gpui-fast`)

1. **Release HLSL Shader Precompilation (`fxc.exe`)**: In `--release` builds, `gpui-fast-windows` and `gpui-pre-windows` run `fxc.exe` in `build.rs` to precompile `shaders.hlsl` (`#include "alpha_correction.hlsl"`). This requires the Windows SDK shader compiler (`fxc.exe`) and filesystem read access to the Cargo registry source directory. Dev/test builds skip build-time `fxc.exe`.
2. **Additive Cargo Features in `gpui-kit 0.11.0`**: Enabling `features = ["gpui-fast"]` switches the active backend to `gpui-fast 0.1.19` cleanly without import changes, but Cargo still compiles `gpui-pre` / `gpui-pre-windows` in the dependency graph. Release LTO + symbol stripping eliminates unused code (`16.23 MB` final EXE).

## Benchmark Comparison (`scripts/measure-perf.ps1`)

Measured on Windows 11 Home (`10.0.26300`), AMD Ryzen 7 5700X3D (8C/16T), 31.93 GB RAM.  
Release profile: `opt-level = "s"`, `lto = "thin"`, `codegen-units = 16`, `strip = "symbols"`.

| Metric | Slint `release-fast` Baseline | `onyx-ui-gpui` (`gpui-fast`) | Delta |
|---|---:|---:|---:|
| **EXE Size** | `21.23 MB` | **`16.23 MB`** (`17,013,760` B) | **-5.00 MB (-23.6%)** |
| **Window-Ready** | `142.99 ms` | **`222.75 ms`** | **+79.76 ms (+55.8%)** |
| **3s Idle WorkingSet64** | `269.43 MB` | **`100.29 MB`** (`105,164,800` B) | **-169.14 MB (-62.8%)** |
| **3s Idle PrivateMemory64** | `455.86 MB` | **`85.71 MB`** (`89,878,528` B) | **-370.15 MB (-81.2%)** |
| **`--smoke-test` Median (5 runs)** | — | `1018.68 ms` (`1014.84–1021.05 ms`) | — |

## Commands

```powershell
cargo fmt --manifest-path crates/onyx-ui-gpui/Cargo.toml -- --check
cargo clippy --manifest-path crates/onyx-ui-gpui/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path crates/onyx-ui-gpui/Cargo.toml
cargo build --release --manifest-path crates/onyx-ui-gpui/Cargo.toml
.\scripts\measure-perf.ps1 -BinaryPath crates\onyx-ui-gpui\target\release\onyx-ui-gpui.exe -Runs 5 -IdleSeconds 3
```
