# Onyx Performance Methodology & Targets

## Targets

| Metric | Target | Notes |
|---|---|---|
| Cold startup | < 300 ms | Measured from `CreateProcess` to first frame / main window ready on release binary |
| Idle memory usage | < 50 MB | Stretch target; working set after opening default Home/Documents directory |
| UI frame rate | 60–120 FPS | GPU-accelerated FemtoVG renderer on Windows 11 |
| Directory scalability | 100,000+ entries | Virtualized `ListView`, streaming batches, Rayon sorting |
| Executable size | < 20 MB | Release profile with `fat` LTO, `opt-level = "s"`, `codegen-units = 1`, and symbol stripping |
| Idle CPU usage | Near zero | Event-driven UI + debounced OS directory watchers |

## Measured Release Binary & Startup Baseline (Windows 11 x64)

Measured directly on the release binary (`onyx.exe` `0.1.0`, `fat` LTO + `opt-level = "s"` + symbol stripping) via `.\scripts\measure-perf.ps1`:

- **Host OS**: Microsoft Windows 11 Home (`10.0.26300`)
- **CPU**: AMD Ryzen 7 5700X3D 8-Core Processor (`8C/16T`)
- **RAM**: `31.93 GB`

| Metric | Measured Result | Notes |
|---|---|---|
| Release executable size | **16.17 MB** (`16,955,904` bytes) | Down from `23.64 MB` (`24,793,088` bytes) pre-optimization; meets `< 20 MB` target |
| `--smoke-test` latency (5 runs) | **88.98 ms** min / **91.32 ms** median / **94.62 ms** max | Runs: `94.62`, `88.98`, `89.93`, `91.32`, `91.73` ms |
| GUI startup (`CreateProcess` -> `MainWindowHandle`) | **50.5 ms** | Meets `< 300 ms` target |
| Idle working set (`WorkingSet64`, 3 s idle) | **143.98 MB** (`150,974,464` bytes) | Includes Slint FemtoVG/OpenGL GPU context + Tokio runtime (above `< 50 MB` stretch target) |
| Idle private bytes (`PrivateMemorySize64`, 3 s idle) | **248.03 MB** (`260,075,520` bytes) | Committed private memory after 3 s idle |

### Runtime & Build Tuning Notes (`feat/perf-worker-limits`)

- **Worker-Pool Limits:** Tokio async runtime is clamped to `2..=4` worker threads (`max_blocking_threads(32)`) and the global Rayon pool is capped at `4` threads (instead of spawning `num_cpus` threads each on 16-thread CPUs).
- **Bounded SQLite Page Caches:** Long-lived SQLite connections cap `PRAGMA cache_size` (`-512` KiB for `MetadataStore` and `FolderSizeStore`; `-2048` KiB for the FTS5 `SearchIndex`).
- **Zero-Syscall Directory Listing:** `onyx-fs` uses `DirEntry::metadata()` first (reusing `WIN32_FIND_DATA` from `FindNextFileW` without an extra `symlink_metadata()` syscall on Windows) with an extended-length fallback.
- **Why `panic = "unwind"` is kept:** `panic = "abort"` was evaluated and intentionally rejected because `onyx-windows/src/dnd.rs` uses `std::panic::catch_unwind` at COM/OLE FFI boundaries to prevent undefined behavior from unwinding across Win32 callbacks.
- **Fast Dev/Test Release Profile (`release-fast`):** Added `[profile.release-fast]` (`lto = "thin"`, `codegen-units = 16`, cold build ~2m 34s vs. ~10–15 min for `fat` LTO) plus `debug = 0` for dev dependencies. Measured on `target/release-fast/onyx.exe`: `21.23 MB` binary, `142.99 ms` startup-to-window-ready, `269.43 MB` working set / `455.86 MB` private bytes in a live Windows session (dominated by Slint FemtoVG/OpenGL driver & active session state — motivating the upcoming `gpui-kit` + `gpui-fast` DirectX 11 spike).

## Measured Criterion Baseline (Windows 11 x64)

Real Criterion run on Windows 11 `10.0.26300.0`, Rust `1.96.1`, Release profile:

| Benchmark | Result |
|---|---|
| `natural_sort/1000` | 1.02 ms |
| `natural_sort/10000` | 18.53 ms |
| `natural_sort/50000` | 114.45 ms |
| `directory_enumeration_1000` | 41.03 ms |
| `lru_directory_cache_put_get` | 5.32 µs |
| `sqlite_fts5_search_1000` | 707.45 µs |

## Reproducible Benchmarking & Measurement

### Release Binary Startup & Memory (`measure-perf.ps1`)

Measure binary size, `--smoke-test` latency, GUI window-ready startup time, and idle memory usage on a release build:

```powershell
.\scripts\measure-perf.ps1
.\scripts\measure-perf.ps1 -BinaryPath D:\Onyx\target\release\onyx.exe -Runs 5 -IdleSeconds 3
```

Options:

| Flag | Description |
|---|---|
| `-BinaryPath <path>` | Path to `onyx.exe` (defaults to `$env:CARGO_TARGET_DIR\release\onyx.exe` or `target\release\onyx.exe`) |
| `-Runs <n>` | Number of `--smoke-test` iterations for min/median/max calculation (default `5`) |
| `-IdleSeconds <n>` | Seconds to keep the GUI window open before sampling `WorkingSet64` and `PrivateMemorySize64` (default `3`) |

### Criterion Micro-Benchmarks

Run Criterion micro-benchmarks in release mode:

```powershell
cargo bench --workspace
```

Included benchmark suites:
1. **Natural sorting** (`1,000`, `10,000`, and `100,000` synthetic filenames with numeric runs and Unicode).
2. **Directory enumeration** (isolated temp directory traversal and metadata collection).
3. **LRU directory cache** lookup & eviction throughput.
4. **Search query matching** (Phase A matcher and Phase B SQLite FTS5 index query latency).

All reported metrics must be measured directly on release builds (`target/release/onyx.exe`) with hardware, OS build, and workload documented alongside results.
