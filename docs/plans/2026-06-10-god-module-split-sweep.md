# God-module split sweep — top 5 refactor candidates

Status: implemented (uncommitted, for review).

## Results

| File | before | after (root) | siblings |
|---|---|---|---|
| `index/mod.rs` | 7893 | 974 | lifecycle, rebuild, incremental, query_api, internals, schema_bootstrap_tests |
| `index/ai.rs` | 2455 | 591 (`ai/mod.rs`) | policy, reconcile, status, store, helpers |
| `index/schema.rs` | 1304 | 251 (`schema/mod.rs`) | baseline, migrations |
| `query/impact.rs` | 1398 | 532 (`impact/mod.rs`) | select, neighbors, items, historical |
| `mcp/tools.rs` | 2201 | 835 (`tools/mod.rs`) | handlers, defaults, tests |

Recipe that worked (after the 1c false start): **keep type definitions in the
module root, move only `fn`/`impl` bodies to siblings.** Siblings are descendants
so they read the root's private struct fields; cross-sibling private fns are
re-exported via `pub(crate) use sibling::*` from the root (fns widened to
`pub(crate)`). Private types appearing in a `pub(crate)` fn signature were widened
to `pub(crate)` to satisfy `private_interfaces`. `impact`'s `graph.rs` sibling was
renamed `neighbors.rs` to avoid colliding with `crate::query::graph`.

No behavior changes. Baseline held throughout: 131 passed, 1 pre-existing
unrelated failure (eval fixtures missing).

## Second tier (re-scan after tier 1)

A re-scan surfaced a milder next tier (the originals had masked them). Split with
the same recipe:

| File | before | after (root) | siblings |
|---|---|---|---|
| `query/memory.rs` | 1632 | 269 | api, resolve, validate |
| `index/github.rs` | 1563 | 360 | api, sync, store, evidence, parse |
| `index/edges.rs` | 1334 | 203 | resolve, extract, helpers |

Notes:
- 13 private types widened to `pub(crate)` (used in moved `pub(crate)` fns).
- `memory_evidence_for_symbol` (0 callers, was incidental public API) restored to
  `pub` + explicit `pub use` so the narrowing didn't silently drop public API.
- `index/mod.rs` still ranks #1 in repo_brief but that is a **churn artifact** (78
  historical commits over a now-961-line curated root), not a size problem.

## Third tier

| File | before | after (root) | siblings |
|---|---|---|---|
| `cli/main.rs` | 1026 | 310 | commands, render, hooks_support |
| `query/graph.rs` | 969 | 285 | traverse, predicates |
| `cli/init.rs` | 872 | 357 | run, scan, render |

Notes:
- `main.rs` is a **bin root** — it can't become a directory, so its siblings live
  alongside it in `cli/src/` and its render/command fns were routed by name (they
  interleave). `main()` + usage + arg helpers stay in `main.rs`.
- `init.rs` keeps its cfg-gated terminal/signal handling + statics in the root.
- 3 private `init` types widened to `pub(crate)`.

Remaining lower-priority candidates (not done): `eval.rs` (785),
`query/repo_brief.rs` (805), `search/lexical.rs` (615) — all judgment calls.



## Goal

Reduce the five highest-scoring god-modules (per `repo_brief` god_modules /
refactor_candidates) into job-focused sibling files, per the repo's own
`rust-modern-style` rule: **`mod.rs` is a curated index, not a junk drawer; split
by job, separate FFI/host-facing `pub` from internal `pub(crate)`.**

This is a **behavior-preserving** refactor. The existing test suite is the
verification — no new tests, no logic changes.

## Baseline (must not regress)

`cargo test --workspace`: **131 passed, 1 failed**. The single failure
(`eval::tests::eval_suite_reports_search_quality_and_current_source_safety`) is
**pre-existing and unrelated** — it `unwrap()`s on missing `evals/queries.toml` /
`evals/expected_hits.toml` fixtures absent from this checkout (eval.rs:776). Bar
for every step: keep 131 passing, introduce zero new failures, `cargo clippy
--all-targets` clean, `cargo fmt`.

## Invariants to respect (from repo memories)

- `index/mod.rs::rebuild_logical_symbols` must keep the wholesale `DELETE FROM`
  (orphan cascade rows collide on the deterministic stable id). Do not "optimize"
  while moving it.
- `index/mod.rs::rebuild_with_progress` bulk PRAGMA block is deliberately only
  `synchronous=OFF` + `cache_size`. Do NOT re-add `journal_mode=MEMORY` /
  `temp_store=MEMORY`. Move verbatim.
- `index/ai.rs` perf note: `ort_threads` is the real CPU lever, not `omp_threads`
  — purely informational; no code change.

## Strategy: preserve public path surface

External callers reference `index::ai::*` (23 sites), `query::impact::*` (15
sites), `mcp::tools::*`, `index::schema::*`. When a single file becomes a
directory (`ai.rs` → `ai/mod.rs`), the new `mod.rs` **re-exports** everything that
was `pub`/`pub(crate)` so external paths stay byte-identical and no caller edits
are needed. Sibling files reach each other via `super::`.

Inherent `impl IndexDatabase` blocks may live in sibling files. A private method
moved to a sibling becomes private to *that* module, so any method called
cross-file must widen to `pub(super)` (visible to the `index` module tree). This
is the main mechanical risk; ratchet visibility to the narrowest that compiles.

Execution order: smallest-risk-first within each file; full build+test+clippy
after each file before moving to the next.

---

## File 1 — `crates/rag-rat-core/src/index/mod.rs` (7893 lines, 341 symbols)

Real structure: mod decls (1–15); DTOs/enums/consts/`IndexError` (63–192);
`impl IndexDatabase` ~110 methods (194–2911); free helpers (2912–3682);
`#[cfg(test)] mod schema_bootstrap_tests` (3683–7893, ~4200 lines).

### 1a. Extract the inline test module (biggest, lowest risk — do first)
Move `schema_bootstrap_tests` (3683–EOF) to `index/schema_bootstrap_tests.rs`,
declared `#[cfg(test)] mod schema_bootstrap_tests;` in mod.rs. Uses `super::*`,
so it keeps resolving. Cuts mod.rs by >50%.

### 1b. Split `impl IndexDatabase` into themed sibling files
Group the methods (inherent-impl blocks via `impl super::IndexDatabase`):
- `index/lifecycle.rs` — open/migrate/create/set_context (195–270).
- `index/rebuild.rs` — full rebuild + clear/delete-cascade (308–491).
  (Holds the two memory-guarded methods — move verbatim.)
- `index/incremental.rs` — index_changed/discover/targets/scopes/plan (492–704).
- `index/query_api.rs` — search/symbols/read_chunk/docs/git/github/ai/graph/
  memory delegation (773–1987). May further split into `query_api.rs` +
  `graph_api.rs` if too large.
- `index/write_internals.rs` — heal_file/index_file/insert_*/resolve_edges/
  rebuild_logical_symbols/fts/meta (1988–2620).
- `index/maintenance.rs` — gc/prune/heal_index/status/discovery_status (705–772,
  1297–1446) + parser-failure/file-ops/anchors (2621–2911).
Widen cross-file-called private methods to `pub(super)`.

### 1c. Free helpers
Move search-hit ranking/classification helpers (2932–3208) to
`index/search_rank.rs`; git/path helpers (3547–3683) to `index/util.rs` (or fold
into the relevant sibling). Row DTOs stay near their users or in a small
`index/rows.rs`.

### 1d. mod.rs result
Curated index: `mod`/`pub mod` decls + `pub use` re-exports of the host-facing
DTOs + the `IndexDatabase` struct/`IndexError` definition. Target < 400 lines.

### OUTCOME (1a–1c done)
- 1a ✅ test module → `index/schema_bootstrap_tests.rs` (4208 lines moved).
- 1b ✅ `impl IndexDatabase` → `lifecycle.rs` / `rebuild.rs` / `incremental.rs` /
  `query_api.rs` / `internals.rs` (each `use super::*; impl IndexDatabase {…}`).
  30 cross-called private methods widened to `pub(super)` (compiler-guided).
- **mod.rs: 7893 → 974 lines.** Build green, clippy clean, tests 131-pass baseline.
- 1c **NOT done — deliberately deferred.** Extracting the row-DTO structs
  (`FileRow`, `FileScope`, `PreparedIndexFile`, …) to a sibling fails cleanly:
  they have private fields that the impl siblings field-access, and Rust's glob
  re-export through `mod.rs` doesn't chain private items. Fixing forward would
  mean widening ~50 items **and their fields** to `pub(crate)` — strictly worse
  than keeping cohesive, tightly-coupled internal types co-located with the
  module index. The 974 lines that remain are: use block + public DTO surface +
  private row DTOs + private support helpers — all genuine index-module internals.
  Leaving < 400 unmet here is the right trade.

---

## File 2 — `crates/rag-rat-core/src/index/ai.rs` (2455 lines, 148 symbols) → `ai/`
- `ai/mod.rs` — curated index + re-exports (preserve `index::ai::*`).
- `ai/embedder.rs` — `Embedder` trait + Hash/FastEmbed/Model2Vec/Mock impls +
  model-id/dim consts + feature messages (13–166).
- `ai/status.rs` — Local/Capability/FastEmbed status, ArtifactCounts,
  ModelInfo, status/capability/artifact-count fns (167–303, 1167–1303, 2296–2398).
- `ai/reconcile.rs` — ReconcilePlan/Options/Progress + reconcile* + finish/
  finalize (277–404, 604–1116).
- `ai/policy.rs` — embedding policy/priority/eligibility + path predicates
  (391–398, 1650–1812).
- `ai/store.rs` — current_chunks/job candidates/batch select/store_embedding/
  write batches (1303–2037).
- `ai/query.rs` — embed_query + hash/encode/decode/tokenize helpers (2037–2295).
- `ai/model.rs` — install/models/model row + manifest/version/meta (404–573,
  1089–1166, 2399–end).

## File 3 — `crates/rag-rat-mcp/src/tools.rs` (2201 lines, 122 symbols) → `tools/`
- `tools/mod.rs` — re-exports + `TOOL_NAMES` + `list_tools`/`call_tool` entry.
- `tools/args.rs` — all `*Args` structs + `Mcp*` enums + their impls/Deserialize
  (22–545).
- `tools/handlers.rs` — `call_tool_with_db` dispatch + each `*_tool` fn +
  selector/coverage helpers (560–1199).
- `tools/schema.rs` — `description`/`schema`/`schema_for`/strip + `default_*`
  (1201–1453).
- keep `#[cfg(test)] mod tests` → `tools/tests.rs`.

## File 4 — `crates/rag-rat-core/src/index/schema.rs` (1304 lines, 73 symbols) → `schema/`
- `schema/mod.rs` — re-exports + public surface (SchemaState/Status,
  LATEST_SCHEMA_VERSION, apply/status/check_compatible) (4–221).
- `schema/baseline.rs` — `apply_baseline` (the 222–713 block) + `rebuild_fts`.
- `schema/migrations.rs` — MIGRATION_* id/checksum/desc consts + every
  `apply_*`/`migrate_*` fn + `applied_migrations`/`known_*`/`record_migration`/
  `add_column_if_missing` helpers (728–end).

## File 5 — `crates/rag-rat-core/src/query/impact.rs` (1398 lines, 66 symbols) → `impact/`
- `impact/mod.rs` — re-exports + public types (ImpactItem, options, report,
  query, completeness, memory-status) + entry points `impact_surface*`,
  `ffi_surface` (11–280).
- `impact/graph.rs` — graph_neighbors/predicate/import-export/siblings (640–855).
- `impact/items.rs` — the `*_items` builders + dedupe/collapse/row mapping
  (902–1172).
- `impact/historical.rs` — git_commits/github_refs/rationale + fts_escape/hash
  helpers (1087–1398).
- `impact/select.rs` — exact_symbols/target-name/candidate/predicate helpers
  (388–636).
