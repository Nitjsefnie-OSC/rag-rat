# rag-rat

`rag-rat` is a local, read-only-source Rust repo-intelligence tool. It builds a SQLite FTS5 index over configured language roots and exposes CLI plus `rmcp` STDIO MCP access for LLM-assisted code search and propagation tracing.

Indexing uses tree-sitter structure for Rust, TypeScript/TSX, and Kotlin source where files are small enough for bounded parsing, markdown heading chunks for docs, and coarse chunks for generated or oversized files. Chunk anchors store content and context fingerprints so stale reads can be detected and repaired.

Graph edges are populated from tree-sitter syntax, not compiler-grade name resolution. The indexer
records pragmatic `imports`, `exports`, `calls_name`, `references_type`, `implements`, and
`contains` edges with confidence labels: `Exact`, `Syntactic`, `NameOnly`, or `Ambiguous`.
This makes `find_callers`, `trace_callees`, and impact routing useful while keeping approximate
edges explicit.

Tree-sitter grammars are exact-pinned in `Cargo.toml` so parser node coverage changes deliberately.

Chunk anchors include normalized text hashes, boundary hashes, nearby context hashes, and an anchor
version. `read_chunk` and search validate anchors against current source, relocate small line drift,
and cap automatic stale-file reindexing per call.

Git history is indexed into SQLite when the target root is a git worktree. Commit subjects/bodies
and path-level changes are historical evidence, separate from current source search. Chunk blame is
computed lazily for current chunk text and cached against the current source text hash.

GitHub papertrail data is fetched only by explicit `github sync` commands through `gh api`; normal
search, papertrail, and rationale tools read the local SQLite cache only. Cached issues, PRs,
comments, reviews, and review comments are indexed as historical GitHub evidence.

Local AI artifacts are explicit. Query paths never download models. `models install` records local
embedding availability, and `reconcile` writes hash-bound 384-dimensional chunk embeddings only for
current chunks. `semantic_search` uses embeddings only when the model is installed, the stored
dimension matches the model metadata, the artifact is `Current`, and the artifact text hash matches
the current chunk text hash. Stale AI artifacts are treated as absent.

CPU-heavy index work uses the Rayon worker pool. File reads, source hashing, tree-sitter preparation,
git-log parsing, and embedding computation run in parallel across available cores; SQLite writes stay
on one deterministic writer path and are transaction-batched where the command owns the write scope.

## Commands

```bash
cargo run --bin rag-rat -- index --config rag-rat.toml
cargo run --bin rag-rat -- index --changed --config rag-rat.toml
cargo run --bin rag-rat -- index --discover --config rag-rat.toml
cargo run --bin rag-rat -- index --full --config rag-rat.toml
cargo run --bin rag-rat -- doctor --config rag-rat.toml
cargo run --bin rag-rat -- migrate --check --config rag-rat.toml
cargo run --bin rag-rat -- migrate --config rag-rat.toml
cargo run --bin rag-rat -- github sync --from-refs --config rag-rat.toml
cargo run --bin rag-rat -- github sync --issue cq27-dev/rag-rat#42 --config rag-rat.toml
cargo run --bin rag-rat -- github sync --from-refs --offline --config rag-rat.toml
cargo run --bin rag-rat -- models list --config rag-rat.toml
cargo run --bin rag-rat -- models install embedding-small --config rag-rat.toml
cargo run --bin rag-rat -- reconcile --limit 100 --config rag-rat.toml
cargo run --bin rag-rat -- eval --config rag-rat.toml
cargo run --bin rag-rat -- eval --json --config rag-rat.toml
cargo run --bin rag-rat -- eval --update-baseline --config rag-rat.toml
cargo run --bin rag-rat -- query --config rag-rat.toml "semantic recall"
cargo run --bin rag-rat -- mcp --config rag-rat.toml
```

By default, rag-rat links against the system SQLite library through `rusqlite`.

## Schema Migrations

The SQLite index has an explicit `schema_version` table. Each migration records an id,
`applied_at_ms`, checksum, and description. Runtime opens are check-only: compatible schemas open,
older schemas report `rag-rat migrate` or `rag-rat index --full`, newer schemas are refused, and
dirty or partial migrations are refused with a rebuild instruction.

Use `migrate --check` for CI/preflight and `migrate` to apply the current index schema baseline.
Because the index is derived, hard migration failures should be resolved with `index --full`.

`index` defaults to `--changed`: it uses git status to index changed/new paths and remove deleted
indexed paths. `index --discover` walks configured targets, detects new files, changed indexed
files, and removed indexed files, then updates only that delta. Use `index --full` to rebuild every
file and rebuild the SQLite FTS5 table from stored chunks. `index --watch` is reserved for a later
file-watcher mode and currently exits with an explicit not-implemented error.

`doctor` reports discovery drift. If configured source files are not indexed, it returns a warning
such as `3 unindexed source files detected. Run rag-rat index --full or rag-rat index --discover.`

Search never runs silently against stale FTS state. The index records a content revision for the
current `files`/`chunks` rows, tracks whether FTS is dirty after writes, and synchronizes FTS before
search when `fts_source_revision` no longer matches `content_revision`.

`eval` runs the fixture-driven ranking and freshness harness from `evals/queries.toml` plus
`evals/expected_hits.toml`. It reports MRR@10, Recall@10, path hit rate, symbol hit rate,
stale-hit rate, current-source violation count, papertrail precision sample, and latency p50/p95.
`stale_current_source_violations` must remain zero. Use `eval --json` for machine-readable output
and `eval --update-baseline` to rewrite `expected_hits.toml` from observed top-10 evidence.

Non-git target roots still index source and docs; git history status reports unavailable with zero
commit/path rows.

GitHub sync discovers references from commits, branch names, indexed files, and docs. It supports
`Fixes`/`Closes`/`Refs`/`See`, `GH-123`, `owner/repo#123`, and full GitHub issue/PR URLs. `--offline`
updates discovered references and reports cache status without network access.

## Configuration

The host repo owns `rag-rat.toml`. This keeps monorepo-specific target bindings out of the reusable tool.

```toml
[index]
root = "."
database = ".rag-rat/index.sqlite"

[target_bindings]
rust = ["core/held-core/src"]
typescript = ["apps/mobile/src", "apps/web/src"]

[[target]]
name = "generated-ts"
language = "typescript"
directories = ["packages/held-core/src/generated"]
kind = "generated"
include = ["**/*.ts"]
```

## Security

The MCP server exposes read-only source tools only. It does not execute shell commands or write configured target files. It may write the configured SQLite index during `index` and during automatic stale-index healing before returning search or `read_chunk` results.

## Size Budget

Storage dependency changes must keep the binary slim. See `docs/binary-size.md` for the
manual size check and heavyweight dependency policy.
