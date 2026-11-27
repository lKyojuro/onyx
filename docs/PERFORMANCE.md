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

## GPUI Release Baseline vs. Performance & Feature Sprint (`zeron/onyx-performance-feature-sprint-a94df3`)

Measurements taken on 2026-11-26 against the `gpui-kit` + `gpui-fast` GPUI binary. Release profile: `fat` LTO, `opt-level = "s"`, `codegen-units = 1`, stripped. **Before** is commit `3ca58fe` (`D:\Onyx\target\release\onyx.exe`), **After** is the sprint build in `target\release\onyx.exe`.

- **Host OS**: Microsoft Windows 11 Home (`10.0.26300`)
- **CPU**: AMD Ryzen 7 5700X3D 8-Core Processor (`8C/16T`)
- **RAM**: `31.93 GB`

### Binary Size, Startup & Idle Memory

| Metric | Before (`3ca58fe`) | After (sprint build) | Delta |
|---|---|---|---|
| Release executable size | `22.79 MB` (`23,891,968` bytes) | `22.93 MB` (`24,044,032` bytes) | `+0.14 MB` |
| `measure-perf.ps1` `--smoke-test`, 5 runs (`Start-Process -Wait`) | min `1005.18 ms` / median `1009.99 ms` / max `1013.80 ms` | min `1006.73 ms` / median `1019.34 ms` / max `1037.81 ms` | flat (measurement floor, see caveat) |
| Direct `.NET Process.WaitForExit` CLI latency | not captured | `--version` **`18.77 ms`**, `--smoke-test` **`16.08 ms`** | — |
| `measure-perf.ps1` GUI startup (`CreateProcess` -> `MainWindowHandle`) | `208.47 ms` | `210.95 ms` | flat |
| Direct `.NET Process::Start` -> `MainWindowHandle` (3 runs, 2 ms poll) | `204.36` / `178.68` / `194.94` ms, median **`194.94 ms`** | `175.93` / `183.53` / `181.07` ms (`--new-window`), median **`181.07 ms`** | **`-7.1%`** |
| Warm single-instance IPC (`onyx.exe <TEMP>` with a running instance, 3 runs) | not implemented | `17.90` / `14.83` / `14.25` ms, median **`14.83 ms`** | **`< 40 ms` target met** |
| Idle `WorkingSet64` (3 s idle) | `75.99 MB` (`79,683,584` bytes) | `77.57 MB` (`81,338,368` bytes) | `+1.58 MB` |
| Idle `PrivateMemorySize64` (3 s idle) | `73.02 MB` (`76,566,528` bytes) | `74.31 MB` (`77,922,304` bytes) | `+1.29 MB` |

**Measurement caveat:** the `measure-perf.ps1` `--smoke-test` numbers are quantized to roughly one second because PowerShell 5.1 `Start-Process -Wait` polls exit status on ~1 s intervals for non-windowed GUI-subsystem processes. The 5-run spread (`1005`-`1038` ms) is therefore PowerShell's polling floor, not Onyx process time. Use `.NET Process.WaitForExit` for real CLI latency: `--version` `18.77 ms` and `--smoke-test` `16.08 ms` describe the actual process lifetime. For GUI startup, poll `MainWindowHandle` from `.NET Process::Start` at 2 ms instead of using the first `WaitForInputIdle`, which is what makes the before/after comparison meaningful (`194.94 ms` -> `181.07 ms`).

**Warm Named Pipe IPC:** the second launch is a client, not a process. `onyx-windows/src/single_instance.rs` probes `\\.\pipe\onyx-explorer-ipc-v1-<user>` with a single `CreateFileW`, writes an `OPEN:<path>` or `SHOW` line, receives the server's PID, and calls `AllowSetForegroundWindow` so the warm instance raises its own window before the client exits. Median warm hand-off is `14.83 ms` against the `< 40 ms` target. The listener claims the pipe with `FILE_FLAG_FIRST_PIPE_INSTANCE`; a cold launch (no listener) pays one `ERROR_FILE_NOT_FOUND` round trip.

### Criterion Transfer Benchmarks

`cargo bench -p onyx-operations --bench transfer_benchmarks`, 10 samples, isolated `tempfile::TempDir` per iteration:

| Benchmark | Workload | Median | Confidence interval | Throughput |
|---|---|---|---|---|
| `Transfer/SmallFilesBatch` | 100 x 8 KiB files | **`48.61 ms`** | `[46.67 ms .. 50.17 ms]` | `16.07 MiB/s` |
| `Transfer/LargeFile16MiB` | 1 x 16 MiB file | **`115.43 ms`** | `[106.84 ms .. 135.38 ms]` | `138.61 MiB/s` |

The small-file batch is dominated by per-file creation cost, which is why the parallel small-file path exists: `onyx-operations` fans small files out across workers sized by `recommended_transfer_workers` and keeps one sequential `CopyFile2` stream for large ones.

### Subsystems Added or Rewired in This Sprint

- **Deferred post-Frame-1 startup (`onyx-ui-gpui/src/explorer.rs`):** `schedule_deferred_startup` runs after the first frame and starts the `IndexController`, the `notify` directory watchers, the clipboard monitor, the lazily created hidden `InputState`s, and the global Rayon pool (`init_rayon_pool`, off the UI thread). Frame 1 no longer waits on any of them; `deferred_started` makes the callback idempotent so on-demand callers (first search, first watcher request) can race it safely.
- **SSD-aware parallel transfer engine (`onyx-windows/src/transfer.rs`):** `detect_storage_media` reads `StorageDeviceSeekPenaltyProperty` to distinguish SSD from HDD, `recommended_transfer_workers` scales parallelism only for seek-penalty-free media, large files go through `CopyFile2` with `COPY_FILE_NO_BUFFERING`, and same-volume ReFS copies attempt `FSCTL_DUPLICATE_EXTENTS_TO_FILE` block cloning before falling back to a byte copy.
- **Low-priority folder size scanner (`onyx-windows/src/fast_scan.rs`, `onyx-fs/src/folder_size.rs`):** enumeration uses `FindFirstFileExW`/`FindNextFileW` directly under `THREAD_MODE_BACKGROUND_BEGIN` so background scans yield CPU and I/O to the foreground, and the scan returns a bottom-up subtree rollup instead of only a root total.
- **Batched folder size cache (`onyx-cache/src/folder_size_store.rs`):** `FolderSizeStore::upsert_batch` writes the whole subtree rollup in one transaction, so a scan costs one write instead of one per folder; `notify`-driven invalidation marks only the affected folder and its ancestors dirty.
- **Shell thumbnails (`onyx-windows/src/thumbnails.rs`, `onyx-preview/src/thumbnails.rs`):** `IShellItemImageFactory::GetImage` produces native thumbnails for videos, PDFs, Office documents, SVG and RAW images, with `SIIGBF_INCACHEONLY` so cloud placeholders are never hydrated behind the user's back.

## Legacy Baseline — Slint UI (`v0.2` era, Windows 11 x64)

Kept for history. These numbers predate the GPUI cutover (see [ADR 0004](decisions/0004-gpui-main-ui.md)) and describe the Slint `onyx-ui` build, not `onyx.exe` today.

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

The transfer engine has its own suite because it exercises Win32 copy paths rather than Rust data structures:

```powershell
cargo bench -p onyx-operations --bench transfer_benchmarks
```

`Transfer/SmallFilesBatch` copies `100 x 8 KiB` files out of an isolated `tempfile::TempDir`, `Transfer/LargeFile16MiB` copies a single 16 MiB file from the same kind of directory. Both report `MiB/s` throughput and must stay inside `TempDir` so no user data is touched.

### Warm single-instance IPC Latency

Start one instance (ideally with `--background` so it keeps running without stealing focus), then time a second launch with `.NET` directly — `Start-Process -Wait` is too coarse for a sub-100 ms hand-off:

```powershell
$exe = "target\release\onyx.exe"
1..3 | ForEach-Object {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $p = [Diagnostics.Process]::Start($exe, "$env:TEMP")
    $p.WaitForExit()
    "{0:N2} ms" -f $sw.Elapsed.TotalMilliseconds
}
```

Compare against the same loop without a running instance to see the cold-start cost of the `CreateFileW` probe.

All reported metrics must be measured directly on release builds (`target/release/onyx.exe`) with hardware, OS build, and workload documented alongside results.
