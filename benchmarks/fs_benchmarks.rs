use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use onyx_cache::DirectoryCache;
use onyx_core::{FileAttributes, FileEntry, FileKind, GenerationId, SortSpec, sort_entries};
use onyx_fs::{EnumerationOptions, FsEngine};
use onyx_search::{SearchIndex, SearchQuery};
use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use tempfile::TempDir;
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

fn make_synthetic_entry(index: usize) -> FileEntry {
    let prefix = if index.is_multiple_of(3) {
        "Module"
    } else if index % 3 == 1 {
        "module"
    } else {
        "Report_Section"
    };
    let num = (index * 37) % 100_000;
    let ext = match index % 4 {
        0 => "rs",
        1 => "txt",
        2 => "md",
        _ => "toml",
    };
    let name = format!("{prefix}_{num}_rev{}.{}", index % 120, ext);
    FileEntry {
        path: PathBuf::from(format!(r"C:\BenchRoot\{name}")),
        name,
        extension: Some(ext.to_string()),
        kind: if index.is_multiple_of(25) {
            FileKind::Directory
        } else {
            FileKind::File
        },
        size: (index as u64) * 128,
        modified: Some(1_700_000_000 + (index as i64)),
        created: Some(1_700_000_000),
        attributes: FileAttributes::default(),
        is_symlink: false,
        symlink_target: None,
        type_label: "File".to_string(),
    }
}

fn build_synthetic_entries(count: usize) -> Vec<FileEntry> {
    (0..count).map(make_synthetic_entry).collect()
}

fn bench_natural_sort(c: &mut Criterion) {
    let mut group = c.benchmark_group("natural_sort");
    let spec = SortSpec::default();

    for size in [1_000_usize, 10_000, 50_000] {
        let template = build_synthetic_entries(size);
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter_batched(
                || template.clone(),
                |mut data| {
                    sort_entries(black_box(&mut data), black_box(spec));
                    data
                },
                BatchSize::LargeInput,
            );
        });
    }

    group.finish();
}

fn bench_directory_enumeration(c: &mut Criterion) {
    let temp = TempDir::new().unwrap_or_else(|e| panic!("failed to create TempDir: {e}"));
    for i in 0..1_000_usize {
        let file_path = temp.path().join(format!("item_{i:04}.txt"));
        fs::write(&file_path, b"benchmark payload").unwrap_or_else(|e| {
            panic!(
                "failed to write benchmark file {}: {e}",
                file_path.display()
            )
        });
    }

    let rt = Runtime::new().unwrap_or_else(|e| panic!("failed to build Tokio runtime: {e}"));
    let engine = FsEngine::new();
    let root_path = temp.path().to_path_buf();
    let options = EnumerationOptions::default();

    c.bench_function("directory_enumeration_1000", |b| {
        b.iter(|| {
            let entries = rt
                .block_on(engine.enumerate_dir(
                    black_box(root_path.clone()),
                    GenerationId::new(1),
                    black_box(options),
                    CancellationToken::new(),
                ))
                .unwrap_or_else(|e| panic!("enumerate_dir failed: {e}"));
            black_box(entries);
        });
    });
}

fn bench_lru_cache(c: &mut Criterion) {
    let cache = DirectoryCache::new(64);
    let sample_entries = build_synthetic_entries(64);
    let paths: Vec<PathBuf> = (0..128_usize)
        .map(|i| PathBuf::from(format!(r"C:\CachedDir\Folder_{i}")))
        .collect();

    for path in paths.iter().take(64) {
        cache.put(path.clone(), sample_entries.clone());
    }

    let mut cursor = 0_usize;
    c.bench_function("lru_directory_cache_put_get", |b| {
        b.iter(|| {
            let idx = cursor % paths.len();
            cursor = cursor.wrapping_add(1);
            let path = &paths[idx];
            if idx.is_multiple_of(4) {
                cache.put(black_box(path.clone()), black_box(sample_entries.clone()));
            } else {
                let hit = cache.get(black_box(path));
                black_box(hit);
            }
        });
    });
}

fn bench_sqlite_fts5_search(c: &mut Criterion) {
    let index = SearchIndex::open_in_memory()
        .unwrap_or_else(|e| panic!("failed to open in-memory SearchIndex: {e}"));

    for i in 0..1_000_usize {
        let entry = make_synthetic_entry(i);
        index
            .upsert_entry(&entry)
            .unwrap_or_else(|e| panic!("failed to upsert benchmark entry: {e}"));
    }

    let query = SearchQuery {
        text: "Module rev".to_string(),
        extensions: vec!["rs".to_string()],
        ..SearchQuery::default()
    };

    c.bench_function("sqlite_fts5_search_1000", |b| {
        b.iter(|| {
            let results = index
                .query(None, black_box(&query))
                .unwrap_or_else(|e| panic!("SearchIndex::query failed: {e}"));
            black_box(results);
        });
    });
}

criterion_group!(
    benches,
    bench_natural_sort,
    bench_directory_enumeration,
    bench_lru_cache,
    bench_sqlite_fts5_search
);
criterion_main!(benches);
