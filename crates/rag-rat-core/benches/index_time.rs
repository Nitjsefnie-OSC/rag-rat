//! Wall-clock full-index time (criterion).
//!
//! Complements the deterministic iai-callgrind instruction counts with a real wall-time signal for
//! a full index rebuild. No callgrind slowdown here, so — unlike the tiny iai subtree — this
//! indexes the **whole** cargo checkout (every `.rs` in the repo, ~1.3k files), which is what
//! `rag-rat index` actually does when pointed at a real repository. Noisier than iai — rely on
//! Bencher's statistical threshold (t-test) to gate regressions. The corpus harness is shared
//! (benches/shared).

mod shared;

use std::time::Duration;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use rag_rat_core::IndexDatabase;
use shared::{bench_config, corpus_dir};

/// Index the entire corpus checkout — the realistic "index this repo" workload, not a cherry-picked
/// subtree. `bench_config` targets every `**/*.rs` under this root.
const SUBDIR: &str = ".";

fn full_index(c: &mut Criterion) {
    // Clone the corpus once, before timing.
    let _ = corpus_dir();

    // Build one index up front to report the real scale being measured (and to size throughput).
    // This is the headline number a user cares about: how big a repo are we actually indexing?
    let probe = bench_config(SUBDIR);
    let db = IndexDatabase::rebuild(&probe).expect("probe rebuild");
    let status = db.status(&probe.database).expect("index status");
    let files: u64 = status.file_count_by_language.values().sum();
    eprintln!(
        "index_time: indexing whole cargo checkout — {files} files {:?}",
        status.file_count_by_language
    );

    let mut group = c.benchmark_group("index_time");
    // Report files/sec, so the bench output shows real indexing throughput, not just opaque
    // latency.
    group.throughput(Throughput::Elements(files));
    // Each rebuild of the full repo takes seconds; take the criterion minimum sample count over a
    // window wide enough to avoid the "couldn't complete in time" warning leaking onto stdout.
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(180));
    group.bench_function("full_rebuild", |b| {
        b.iter_batched(
            || bench_config(SUBDIR),
            |config| {
                let db = IndexDatabase::rebuild(&config).expect("rebuild corpus index");
                std::hint::black_box(db);
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

criterion_group!(benches, full_index);
criterion_main!(benches);
