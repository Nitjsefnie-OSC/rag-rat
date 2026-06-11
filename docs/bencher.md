# Continuous benchmarking (Bencher)

rag-rat tracks performance over time with [Bencher](https://bencher.dev). Two lightweight signals
are recorded on every push to `main` and gated on every pull request; a third, heavyweight signal
runs only on a published release:

| Signal | Harness | When | What it measures | Why |
|---|---|---|---|---|
| `rag_pipeline` | [iai-callgrind](https://github.com/iai-callgrind/iai-callgrind) | push + PR | CPU **instruction counts** for index rebuild + cold/warm query | Deterministic — immune to CI runner noise, so it catches small *relative* regressions a wall-clock bench would drown out. |
| `index_time` | [criterion](https://github.com/bheisler/criterion.rs) | push + PR | **Wall-clock** time for a full index rebuild (whole cargo checkout) | The number users actually feel. Noisy, so it's gated statistically (t-test), not on a tight percentage. |
| `linux-kernel-v7.0/full-index` | single-shot ([`tools/bench-kernel.sh`](../tools/bench-kernel.sh)) | **release only** | **Wall-clock + throughput + peak memory** to index the whole Linux kernel | The headline "indexes the Linux kernel in X seconds" number on a huge real C codebase. Too slow (~tens of minutes) for per-push. |

The push/PR harnesses run on different corpus sizes on purpose — see "Corpus" below.

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

Four workflows under `.github/workflows/`:

| Workflow | Trigger | Has the token? | Role |
|---|---|---|---|
| `bench.yml` | push to `main` | yes | Records both lightweight signals against the `main` branch; sets/refreshes the thresholds new PRs are compared against. |
| `bench-pr-run.yml` | `pull_request` (incl. forks) | **no** | Runs the benches, uploads raw output + the PR event as an artifact. Never sees secrets, so a fork PR can't exfiltrate the token. |
| `bench-pr-track.yml` | `workflow_run` of bench-pr-run | yes | Runs in the base-repo context: downloads the artifact, uploads results to Bencher, comments the comparison on the PR. |
| `bench-release.yml` | `release: published` + manual `workflow_dispatch` | yes | The heavyweight Linux-kernel headline bench (below). Not a gate — tracks latency/throughput/memory over releases. |

The `pull_request` → `workflow_run` split is Bencher's documented fork-safe pattern: untrusted fork
code runs without secrets, and only the trusted base-repo workflow (which never executes fork code)
holds the API token. PR runs use `--start-point main --start-point-clone-thresholds
--start-point-reset` so each PR is measured against a fresh clone of `main`'s baseline + thresholds.

## Release headline: indexing the Linux kernel

`tools/bench-kernel.sh` (run by `bench-release.yml`) is the "indexes the Linux kernel in X seconds"
benchmark. It shallow-clones a pinned kernel tag (`KERNEL_TAG`, default `v7.0`), indexes its C/H
sources **once** with the release `rag-rat` binary (`index --full`, `--no-default-features` =
hash embedder, no model download), and writes a Bencher Metric Format JSON file with three measures:

- `latency` — wall-clock seconds to index, in nanoseconds (Bencher's built-in Latency measure).
- `throughput` — indexed files per second.
- `memory` — peak resident set size in bytes (from `/usr/bin/time -v`).

It's a **single cold rebuild**, not a criterion loop: the whole kernel is ~63k C/H files and takes
tens of minutes, so criterion's 10-sample minimum (×10) is a non-starter. One run is also exactly
the number a user sees.

Run it locally or trigger it manually before relying on it for a release:

```bash
cargo build --release --no-default-features --bin rag-rat
RAG_RAT_BIN=target/release/rag-rat bash tools/bench-kernel.sh   # whole tree (~tens of min, several GB RAM)

# Bound the scope (faster, less memory) while iterating:
RAG_RAT_KERNEL_SUBDIRS="kernel mm fs net lib" RAG_RAT_BIN=target/release/rag-rat bash tools/bench-kernel.sh
```

Or in CI: **Actions → bench-release → Run workflow** (the `workflow_dispatch` input sets the subtree).

**Runner sizing.** Memory scales sublinearly (mostly fixed overhead + the cross-file symbol/edge
graph), but the whole tree still lands in the multi-GB range — borderline for the standard 7 GB
`ubuntu-latest` runner. If a release run OOMs or overruns `timeout-minutes`, either bound
`RAG_RAT_KERNEL_SUBDIRS` to the core subsystems or move the job to a larger runner. Validate with a
manual dispatch first.

## One-time setup

1. **Create the Bencher project.** On [bencher.dev](https://bencher.dev), create a project named
   `rag-rat` (must match `BENCHER_PROJECT` in the workflows). The default `ubuntu-latest` testbed
   matches `BENCHER_TESTBED`.
2. **Add a project key as a repo secret.** In Bencher, create a **project API key** for the
   `rag-rat` project (a token that starts with `bencher_run_`; project keys are scoped to one
   project and limited to `bencher run` and other non-destructive commands). Add it to the GitHub
   repo as the secret `BENCHER_API_TOKEN` (Settings → Secrets and variables → Actions). The
   workflows surface it to the CLI as **`BENCHER_API_KEY`** — the `--key` auth path. Do *not* use
   an account API token (a JWT starting with `eyJ`): the CLI would expect that under
   `BENCHER_API_TOKEN`/`--token` and a project key fails JWT validation there. `GITHUB_TOKEN` for
   PR comments is provided automatically by Actions.
3. **Push to `main`.** The first `bench.yml` run seeds the baseline; subsequent PRs are gated
   against it.

If you change the iai-callgrind library version in `Cargo.toml`, bump `IAI_RUNNER_VERSION` in
`bench.yml` and the `iai-runner-<version>` cache key in the two PR workflows to match.
