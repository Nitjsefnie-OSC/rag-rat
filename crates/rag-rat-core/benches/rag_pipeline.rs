//! Instruction-count benchmarks for the rag-rat pipeline (iai-callgrind).
//!
//! These measure deterministic CPU instruction counts (via callgrind), not wall time, so they are
//! immune to CI noise and good for catching *relative* regressions. Needs `valgrind` + the
//! matching `iai-callgrind-runner` on PATH (`cargo install iai-callgrind-runner@<lib version>`).
//!
//! Corpus: a real Rust codebase (cargo at tag 0.97.1), shallow-cloned and pinned by commit SHA so
//! the measurement is reproducible. Only a bounded subtree is indexed so the callgrind run stays
//! tractable. The clone is cached (CI caches the corpus dir); cloning + index setup happen OUTSIDE
//! the measured region.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{env, fs};

use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use rag_rat_core::config::{ResolvedTarget, TargetKind};
use rag_rat_core::language::Language;
use rag_rat_core::{Config, IndexDatabase};

const CORPUS_REPO: &str = "https://github.com/rust-lang/cargo.git";
/// cargo tag 0.97.1 — pinned by commit SHA for reproducibility.
const CORPUS_SHA: &str = "fc1044d6129608b3a3188566a919dc6126f7cb15";
/// A small but representative subtree. Kept deliberately tiny because iai-callgrind runs even the
/// (uncounted) `setup` index builds under valgrind's ~50x slowdown — a larger subtree makes the
/// suite take many minutes. The instruction count is a deterministic regression signal regardless
/// of corpus size.
const CORPUS_SUBDIR: &str = "src/cargo/core/resolver";
const QUERY: &str = "resolve dependency version conflict";

fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
    assert!(status.success(), "git {args:?} failed");
}

/// Shallow-clone the corpus pinned to `CORPUS_SHA` into a cached dir, once. Idempotent — a present
/// checkout is reused (CI caches this path). Override the base dir with `RAG_RAT_BENCH_CORPUS`.
fn corpus_dir() -> PathBuf {
    let base = env::var_os("RAG_RAT_BENCH_CORPUS").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/bench-corpus")
    });
    let dir = base.join(format!("cargo-{}", &CORPUS_SHA[..12]));
    if !dir.join(CORPUS_SUBDIR).exists() {
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create corpus dir");
        run_git(&dir, &["init", "-q"]);
        run_git(&dir, &["remote", "add", "origin", CORPUS_REPO]);
        run_git(&dir, &["fetch", "--depth", "1", "-q", "origin", CORPUS_SHA]);
        run_git(&dir, &["checkout", "-q", CORPUS_SHA]);
    }
    dir
}

static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_db_path() -> PathBuf {
    let n = DB_COUNTER.fetch_add(1, Ordering::Relaxed);
    env::temp_dir().join(format!("rag-rat-bench-{}-{n}.sqlite", std::process::id()))
}

/// A Config indexing the bounded corpus subtree into a fresh temp DB. Built in `setup` (outside
/// the measured region).
fn bench_config() -> Config {
    Config {
        root: corpus_dir(),
        database: temp_db_path(),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from(CORPUS_SUBDIR)],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        local_ai: Default::default(),
        watch: Default::default(),
    }
}

/// Build a fresh index and return the open handle (setup for warm-query).
fn built_index() -> IndexDatabase {
    IndexDatabase::rebuild(&bench_config()).expect("rebuild corpus index")
}

/// Build a fresh index and return its on-disk path (setup for cold-open query).
fn built_index_path() -> PathBuf {
    let config = bench_config();
    IndexDatabase::rebuild(&config).expect("rebuild corpus index");
    config.database
}

// Index throughput: full rebuild of the corpus subtree. Setup clones the corpus + builds the
// Config (not measured); only `rebuild` is measured.
#[library_benchmark]
#[bench::cargo_core(setup = bench_config)]
fn index(config: Config) -> IndexDatabase {
    IndexDatabase::rebuild(&config).expect("rebuild corpus index")
}

// Cold query latency: open a freshly-built index from disk (cold page cache) and run one search.
#[library_benchmark]
#[bench::cargo_core(setup = built_index_path)]
fn query_cold(db_path: PathBuf) -> usize {
    let db = IndexDatabase::open(&db_path).expect("open index");
    db.search(QUERY, 10, false).expect("search").len()
}

// Warm query latency: search against an already-open index (warm caches). The index build is in
// setup (not measured).
#[library_benchmark]
#[bench::cargo_core(setup = built_index)]
fn query_warm(db: IndexDatabase) -> usize {
    db.search(QUERY, 10, false).expect("search").len()
}

library_benchmark_group!(
    name = pipeline;
    benchmarks = index, query_cold, query_warm
);

main!(library_benchmark_groups = pipeline);
