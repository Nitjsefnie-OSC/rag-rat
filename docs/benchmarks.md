# Measured benchmarks

Concrete numbers for the headline workload — indexing the whole Linux kernel — plus the memory
profile that workload exposes. This is the *results* companion to [`bencher.md`](./bencher.md),
which documents the *harness* (Bencher, the CI workflows, and `tools/bench-kernel.sh`). Numbers here
are single cold runs on a self-hosted box, not statistically-gated CI signals; treat them as
"what one run looks like," not a regression gate.

## Test harness

All numbers below come from one machine — the self-hosted Bencher testbed `hetzner-bigmem` (the
`bench-release` workflow's `[self-hosted, bigmem]` runner). The kernel index is run here rather than
on a hosted runner for a stable, uncontended wall-clock, not because it no longer fits a hosted box
(the peak is 5.5 GiB; see below).

| | |
|---|---|
| CPU | AMD Ryzen 5 3600 — 6 cores / 12 threads, up to ~4.2 GHz |
| RAM | 64 GB (62 GiB) DDR4 (+ 31 GiB swap present; not monitored during the run) |
| Storage | 2× Samsung MZVLB512HBJQ NVMe SSD in Linux mdraid **RAID1**, ext4 root — kernel checkout + index DB both on NVMe (`KERNEL_WORK` set off the box's RAM-backed `/tmp`) |
| OS / kernel | Arch Linux, Linux 6.18.7 |
| Toolchain | rustc/cargo 1.96.0, git 2.54.0 |

The rebuild's per-wave prepare stage is parallel (rayon), but the bulk of the wall-clock — the edge
insert + index rebuild + FTS pipeline (~t+133–636 s below) — is serial and storage-bound, so storage
speed dominates wall-clock more than core count does. Peak RSS is governed by the graph/chunk set
and `RAG_RAT_INDEX_WAVE`, not by core count.

## Headline: indexing the Linux kernel

Linux kernel **v7.0** (pinned at commit `028ef9c96e96197026887c0f092424679298aae8`, shallow-cloned),
full index (`index --full`), hash embedder (`--no-default-features`, no model download), release
build.

| Metric | Value |
|---|---|
| Files indexed (C/H) | 62,903 |
| Wall-clock | 738.5 s |
| Throughput | 85.2 files/s |
| **Peak RSS** | **5.50 GiB** (5,905,502,208 B maxrss; 1 Hz sampler agrees) |
| Symbols | 3,536,897 |
| Edges (call graph) | 11,213,107 |
| Edges resolved | 7,557,476 (67.4%) |
| Chunks | 4,246,140 |

Unresolved-edge taxonomy (the 32.6% / 3,655,631 edges the graph leaves dangling, by kind):
`calls_name` 2,335,838 (63.9%), `references_type` 917,941 (25.1%), `imports` 401,852 (11.0%). Per
`tools/bench-kernel.sh`, the `calls_name` bucket is extern / macro / function-pointer call targets
the syntactic resolver can't bind without a compilation database — see issue #61 (SCIP oracle).

## Memory profile: where the peak lives

The run has **two** memory humps, and the higher one is missed by the named probes. The per-phase
`RAG_RAT_MEM_TRACE=1` probes instrument the rebuild transaction and stop at COMMIT; they do **not**
cover the embedding reconcile that `index --full` runs afterward. The 1 Hz `/proc` VmRSS sampler
covers the whole process and is what catches the true peak. (Values below are GiB; MEMTRACE prints
them labeled "GB" but computes KiB/1024², i.e. GiB.)

Rebuild transaction (MEMTRACE, `t` from transaction start):

```
before clear (start of rebuild txn):                  t+0s    rss=0.01
edges: symbols hydrated + index built, before insert: t+133s  rss=4.64
edges: inserted, before index rebuild:                t+479s  rss=4.64
edges: after index rebuild:                           t+520s  rss=4.66   <- rebuild's own ceiling
after index_targets (edges resolved+inserted):        t+523s  rss=2.99   <- in-memory graph freed
after rebuild_logical_symbols:                        t+574s  rss=2.99
after rebuild_fts:                                    t+636s  rss=2.99
after COMMIT:                                         t+698s  rss=2.99
```

The rebuild's own ceiling is the **edge-resolution window** (t+133–520 s): the whole symbol + edge
graph is held in memory until the single resolve-and-insert pass, ~4.6 GiB. Once `index_targets`
frees it, RSS settles to ~3.0 GiB and stays flat through logical symbols, FTS, and COMMIT.

But the **process peak is higher and comes later** — the VmRSS sampler shows a second hump *after*
COMMIT, during the embedding reconcile (the hash embedder is always ready, so embeddings are
actually computed):

```
sampler t+719.5s  rss=2.99 GiB   (post-COMMIT baseline)
        t+721.5s  rss=4.04
        t+723.5s  rss=5.23
        t+724.5s  rss=5.50       <- reconcile peak, held a sustained ~11 s
        t+735.5s  rss=5.50
        t+736.5s  rss=3.28       (reconcile done; chunk rows freed)
```

The sampler clock starts at process spawn, ~22 s ahead of the MEMTRACE transaction clock (DB open,
migration, discovery run first), so sampler t+720 ≈ MEMTRACE "after COMMIT" t+698. The peak is a
sustained ~11 s plateau at **5.50 GiB**, not a sub-second transient: the reconcile materializes
chunk rows to embed them, ~2.5 GiB on top of the ~3.0 GiB resident baseline. maxrss and the sampled
plateau agree.

So the ceiling is the **embedding reconcile**, with the rebuild's edge phase (4.66 GiB) a close
second. (An earlier projection had the edge phase becoming the ceiling once the reconcile was fixed;
the measured run corrects that — the fix lowered the reconcile but did not dethrone it.)

### How the peak got here (~9.3 GiB → 5.5 GiB)

The measured drop is the **streaming reconcile fix** (`d5b834e`). The reconcile used to count
policy-skipped chunks by materializing *every* chunk row — including each chunk's full `text` — into
a `Vec`, ~4 GiB resident purely for a count, even when zero chunks end up embedded. Streaming the
count row-by-row removed that ~4 GiB (the isolated skip-summary dropped 3950 MB → 11 MB, counts
identical). What remains at 5.5 GiB is the reconcile's *actual* embedding materialization — the next
lever is to stream the embedding job the same way it now streams the count.

Separately and earlier, the rebuild's edge phase was made ~4 GiB cheaper by interning the edge
accumulator to `Sym(u32)` ids (`CompactEdge` ≈ 64 B vs 176 B), verified byte-identical against a
golden index. That win was measured on smaller corpora (~1% delta there) and modeled at kernel
scale, not isolated in a kernel run; it sets the 4.6 GiB edge-phase baseline above.

Ruled out as the peak, each on the real artifact rather than by reasoning:

- The **SQLite checkpoint is flat at ~28 MB**, shown three ways (default cache, 256 MB cache,
  `synchronous=OFF`) replaying the real 9.5 GB WAL. `mmap_size=0`, no hooks.
- **glibc arenas** account for only ~1.2 GiB (`MALLOC_ARENA_MAX=1`, a live-process malloc setting).

## Knobs

- `RAG_RAT_INDEX_WAVE` (default 2000) — full-rebuild wave size: files are prepared in parallel
  waves, so the rebuild peak ≈ one wave of prepared files + the accumulating graph. Lower it to
  trade speed for peak RSS on a memory-constrained box.
- `RAG_RAT_MEM_TRACE=1` — emit the per-phase rebuild RSS + sqlite-memory curve above to stderr.
  (Note: it does not instrument the post-COMMIT reconcile — use the sampler CSV for that.)
- `RAG_RAT_KERNEL_SUBDIRS` (bench only) — bound the indexed subtree to go faster while iterating.

See [`bencher.md`](./bencher.md) and `tools/bench-kernel.sh` for running it yourself or in CI.

## Note on git-history depth

The kernel bench shallow-clones (one commit of history), so it stresses *file count*, not *history
depth* — these are independent axes. Git-history indexing reads the full reachable history
(`git log --numstat`, O(total history)); on a deep-history repo that cost is gated to run only when
HEAD actually changes (`git_history::is_history_current`), so the steady-state watcher cost does not
scale with history depth.
