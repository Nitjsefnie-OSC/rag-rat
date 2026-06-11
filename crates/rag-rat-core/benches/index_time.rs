//! Wall-clock full-index time (criterion).
//!
//! Complements the deterministic iai-callgrind instruction counts with a real wall-time signal for
//! a full index rebuild. No callgrind slowdown, so this indexes a larger corpus subtree than the
//! instruction-count benches. Noisier than iai — rely on Bencher's statistical threshold (t-test)
//! to gate regressions. The corpus harness is shared (benches/shared).

mod shared;

use std::time::Duration;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use rag_rat_core::IndexDatabase;
use shared::{bench_config, corpus_dir};

/// A larger subtree than the instruction-count benches — a meaningful "full index" wall time.
const SUBDIR: &str = "src/cargo/core";

fn full_index(c: &mut Criterion) {
    // Clone the corpus once, before timing.
    let _ = corpus_dir();
    let mut group = c.benchmark_group("index_time");
    // Each rebuild is expensive (seconds), so take few samples over a bounded window.
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));
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
