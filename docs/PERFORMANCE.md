# Onyx Performance Methodology & Targets

## Targets

| Metric | Target | Notes |
|---|---|---|
| Cold startup | < 300 ms | Measured from `CreateProcess` to first frame on release binary |
| Idle memory usage | < 50 MB | Working set after opening default Home/Documents directory |
| UI frame rate | 60–120 FPS | GPU-accelerated FemtoVG renderer on Windows 11 |
| Directory scalability | 100,000+ entries | Virtualized `ListView`, streaming batches, Rayon sorting |
| Executable size | < 20 MB | Release profile with thin LTO and symbol stripping |
| Idle CPU usage | Near zero | Event-driven UI + debounced OS directory watchers |

## Reproducible Benchmarking

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
