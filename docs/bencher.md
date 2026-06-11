# Continuous benchmarking (Bencher)

rag-rat tracks performance over time with [Bencher](https://bencher.dev). Two independent signals
are recorded on every push to `main` and gated on every pull request:

| Signal | Harness | What it measures | Why |
|---|---|---|---|
| `rag_pipeline` | [iai-callgrind](https://github.com/iai-callgrind/iai-callgrind) | CPU **instruction counts** for index rebuild + cold/warm query | Deterministic — immune to CI runner noise, so it catches small *relative* regressions a wall-clock bench would drown out. |
| `index_time` | [criterion](https://github.com/bheisler/criterion.rs) | **Wall-clock** time for a full index rebuild | The number users actually feel. Noisy, so it's gated statistically (t-test), not on a tight percentage. |

The two harnesses run on different corpus sizes on purpose — see "Corpus" below.

## Benches

Both benches live in `crates/rag-rat-core/benches/` and share a corpus harness
(`benches/shared/mod.rs`, kept in a subdirectory so cargo doesn't auto-detect it as a bench):

- `rag_pipeline.rs` (iai-callgrind, `harness = false`) — three `#[library_benchmark]`s: `index`
  (rebuild), `query_cold` (`open` + search from disk), `query_warm` (search on an open db).
- `index_time.rs` (criterion, `harness = false`) — one `full_rebuild` benchmark.

Run them locally:

```bash
# Instruction counts (needs valgrind + iai-callgrind-runner on PATH — see below)
cargo bench --no-default-features --bench rag_pipeline

# Wall-clock full-index time
cargo bench --no-default-features --bench index_time
```

`--no-default-features` builds the **hash embedder only** — no ONNX / model downloads, no network,
fully deterministic. The benches assert nothing about embedding *quality*; they measure the
indexing and retrieval *machinery*, which must stay model-agnostic to be reproducible in CI.

### Local prerequisites for the iai bench

iai-callgrind runs the benched code under valgrind/callgrind and needs a matching runner binary:

```bash
sudo apt-get install -y valgrind                     # or your distro's package
cargo install --locked iai-callgrind-runner@0.16.1   # MUST match the iai-callgrind lib version in Cargo.toml
```

The criterion bench has no special prerequisites.

## Corpus

Both benches index a pinned snapshot of an external repo so the workload is realistic and stable
across runs. The corpus is **not vendored into git**; the harness shallow-clones it on first use and
caches it under `target/bench-corpus/`:

- Repo: `rust-lang/cargo`, pinned by **commit SHA** (tag `0.97.1`).
- Shallow fetch of just that commit (`git init` + `fetch --depth 1 origin <sha>`), so the clone is
  small and the content can never drift.
- Override the location with `RAG_RAT_BENCH_CORPUS=/path/to/checkout` to point at an existing
  checkout (useful offline).

The iai bench indexes a **small** subtree (`src/cargo/core/resolver`, ~10 files) because callgrind's
~50x slowdown applies even to the uncounted `setup` index builds — a large subtree would time the
job out. The criterion bench indexes the **whole checkout** (~1.3k `.rs` files) — the realistic
"index this repo" workload `rag-rat index` actually performs — since it runs at native speed; it
reports the indexed `file_count_by_language` and files/sec throughput so the output shows real usage.
If you bump the pinned SHA, update the cache key (`bench-corpus-cargo-<version>`) in all three
workflow files too.

## CI workflows

Three workflows under `.github/workflows/`:

| Workflow | Trigger | Has the token? | Role |
|---|---|---|---|
| `bench.yml` | push to `main` | yes | Records both signals against the `main` branch; sets/refreshes the thresholds new PRs are compared against. |
| `bench-pr-run.yml` | `pull_request` (incl. forks) | **no** | Runs the benches, uploads raw output + the PR event as an artifact. Never sees secrets, so a fork PR can't exfiltrate the token. |
| `bench-pr-track.yml` | `workflow_run` of bench-pr-run | yes | Runs in the base-repo context: downloads the artifact, uploads results to Bencher, comments the comparison on the PR. |

The `pull_request` → `workflow_run` split is Bencher's documented fork-safe pattern: untrusted fork
code runs without secrets, and only the trusted base-repo workflow (which never executes fork code)
holds the API token. PR runs use `--start-point main --start-point-clone-thresholds
--start-point-reset` so each PR is measured against a fresh clone of `main`'s baseline + thresholds.

## One-time setup

1. **Create the Bencher project.** On [bencher.dev](https://bencher.dev), create a project named
   `rag-rat` (must match `BENCHER_PROJECT` in the workflows). The default `ubuntu-latest` testbed
   matches `BENCHER_TESTBED`.
2. **Add the API token as a repo secret.** Generate an API token in Bencher
   (Account → API Tokens) and add it to the GitHub repo as `BENCHER_API_TOKEN`
   (Settings → Secrets and variables → Actions). All three relevant jobs read it from there;
   `GITHUB_TOKEN` for PR comments is provided automatically by Actions.
3. **Push to `main`.** The first `bench.yml` run seeds the baseline; subsequent PRs are gated
   against it.

If you change the iai-callgrind library version in `Cargo.toml`, bump `IAI_RUNNER_VERSION` in
`bench.yml` and the `iai-runner-<version>` cache key in the two PR workflows to match.
