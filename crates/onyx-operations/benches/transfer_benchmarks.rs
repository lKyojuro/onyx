//! End-to-end file copy benchmarks using isolated source and destination directories.

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use onyx_operations::{ConflictPolicy, FileOperationsEngine};
use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use tempfile::TempDir;
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

fn fixture(file_count: usize, bytes_per_file: usize) -> std::io::Result<(TempDir, Vec<PathBuf>)> {
    let dir = TempDir::new()?;
    let payload = vec![0xab; bytes_per_file];
    let mut paths = Vec::with_capacity(file_count);
    for index in 0..file_count {
        let path = dir.path().join(format!("file-{index}.bin"));
        fs::write(&path, &payload)?;
        paths.push(path);
    }
    Ok((dir, paths))
}

fn transfer_benchmarks(criterion: &mut Criterion) {
    let runtime = Runtime::new().unwrap_or_else(|err| panic!("Create benchmark runtime: {err}"));
    let engine = FileOperationsEngine::new();
    let mut group = criterion.benchmark_group("Transfer");

    for (name, count, size) in [
        ("SmallFilesBatch", 100, 8 * 1024),
        ("LargeFile16MiB", 1, 16 * 1024 * 1024),
    ] {
        let (_source_dir, paths) =
            fixture(count, size).unwrap_or_else(|err| panic!("Create benchmark source: {err}"));
        group.throughput(Throughput::Bytes((count * size) as u64));
        group.bench_function(name, |b| {
            b.iter_batched(
                || {
                    let destination = TempDir::new()
                        .unwrap_or_else(|err| panic!("Create benchmark destination: {err}"));
                    (destination, paths.clone())
                },
                |(destination, sources)| {
                    let report = runtime
                        .block_on(engine.copy_items(
                            &sources,
                            destination.path(),
                            ConflictPolicy::Skip,
                            CancellationToken::new(),
                            None,
                        ))
                        .unwrap_or_else(|err| panic!("Benchmark copy failed: {err}"));
                    assert!(report.failed.is_empty(), "{:?}", report.failed);
                    assert_eq!(report.succeeded.len(), count);
                    black_box(report);
                    destination
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, transfer_benchmarks);
criterion_main!(benches);
