# rag-rat

`rag-rat` is a local repo-intelligence index and MCP server for coding agents. It keeps source
files read-only, writes only its configured SQLite database, and exposes current source, graph,
git, GitHub papertrail, local-AI artifact status, and source-anchored repo memories as evidence.

It is built for agents that need more than `rg` but still need local, inspectable provenance:

- current source chunks with stale-anchor validation
- Rust, TypeScript/TSX, Kotlin, and Markdown structure
- tree-sitter-derived call/reference/import/export graph edges
- git history, lazy chunk blame, and path-level commit evidence
- cached GitHub issue/PR/review/comment rationale
- local embedding model bookkeeping and reconciliation
- symbol/edge/path-bound repo memories that surface during future queries

GPT-5.5's take:

> Keep it in the default toolbox. Use `rag-rat` first for "where is this concept implemented?",
> "why was this decision made?", "what historical PR/comment explains this?", and "what calls
> this?". For final correctness, still verify with direct file reads and targeted tests.

## Supported Today

### Source Indexing

`rag-rat` indexes configured repository targets into SQLite. It supports:

- Rust, TypeScript, TSX, Kotlin, and Markdown
- generated/coarse targets for large or generated files
- tree-sitter symbols and chunks for supported source languages
- Markdown heading chunks for docs
- parser failure tracking, file counts, and index freshness reporting
- changed-file, discovery, and full-rebuild index modes

Index rows are context-aware for git worktrees. Clean files are stored by `commit_sha`; dirty or
untracked files are stored under a worktree overlay. Queries prefer the active worktree overlay and
fall back to the active commit, so a single database can reuse rows across branch switches while
still reflecting uncommitted local edits.

### Current-Source Safety

Chunks store text hashes, boundary hashes, context hashes, and an anchor version. `read_chunk` and
search validate indexed hits against current source before returning them. Small line drift can be
relocated; larger rewrites are reported as stale or gone. SQLite FTS is refreshed when the stored
content revision says it is dirty.

### Graph Intelligence

The graph is tree-sitter-derived, not compiler-grade. Edges are stored with explicit confidence and
provenance:

- edge kinds: `calls_name`, `constructs`, `uses_macro`, `references_type`, `imports`, `exports`,
  `contains`, `implements`
- confidence labels: `Exact`, `Syntactic`, `NameOnly`, `Ambiguous`
- callsite path/span, raw evidence snippets, receiver hints, target names, resolved symbol ids, and
  resolution reasons

`trace_callees` defaults to call-like edges (`calls_name` and `constructs`) so type references and
macro/module collisions do not look like normal callees unless requested. Duplicate cfg-gated Rust
definitions are grouped as logical symbols, so agents can ask for one logical API without falling
back to unsafe bare-name matching.

### Search And Impact

The MCP surface includes:

- `semantic_search`: indexed source/docs recall with SQLite BM25 lexical search and stale-hit
  validation
- `symbol_lookup`: exact or fuzzy Rust/TypeScript/Kotlin symbol lookup
- `find_callers` and `trace_callees`: reverse/forward graph traversal
- `compare_graph_to_text`: graph caller edges compared against regex text hits
- `impact_surface`: coding preflight that combines graph, optional text fallback, docs, git,
  GitHub papertrail, tests, and repo memories
- `docs_for_symbol`: documentation chunks related to a symbol
- `read_chunk`: current text for a selected chunk with anchor validation

The name `semantic_search` is historical: the current supported MCP behavior is lexical BM25 recall
plus freshness checks. Local embedding infrastructure exists, but vector recall should be treated as
model/artifact-dependent rather than guaranteed.

### Git And GitHub Evidence

When the target root is a git worktree, `rag-rat` indexes commit subjects, bodies, and touched
paths. It also computes chunk blame lazily and caches blame against the current chunk text hash.

Supported MCP tools:

- `commit_search`
- `git_history_for_path`
- `git_history_for_symbol`
- `commits_touching_query`
- `git_blame_chunk`

GitHub papertrail is cache-first. `github sync` uses `gh api` explicitly; normal MCP tools read only
the SQLite cache. Cached issues, PRs, issue comments, PR reviews, and review comments are indexed as
historical rationale.

Supported MCP tools:

- `papertrail_for_chunk`
- `papertrail_for_symbol`
- `papertrail_for_commit`
- `github_issue_search`
- `github_refs_for_path`
- `rationale_search`
- `github_sync_status`

Reference discovery supports common issue forms such as `Fixes #123`, `GH-123`,
`owner/repo#123`, and full GitHub issue/PR URLs.

### FFI Discovery

`ffi_surface` finds likely FFI-relevant rows with evidence classes:

- Rust UniFFI/exported items
- native binding references
- generated binding artifacts

This is a discovery/preflight tool, not a proof of ABI compatibility.

### Local AI Artifacts

Local AI state is explicit and inspectable:

- `embedding-hash`: deterministic baseline embedder
- `fastembed-all-minilm-l6-v2`: local FastEmbed backend when built with the `fastembed` feature
- `models list/install`: model registry and install state
- `local_ai_status`: active/installed/missing status plus chunk/vector counters
- `reconcile`: derived-artifact queue for embedding current eligible chunks

`reconcile` embeds only eligible current chunks whose bounded embedding input is missing, stale by
input hash, stale by model/version/dimension, or retryable after failure. Low-signal chunks are
skipped with explicit policy reasons such as `SkipGenerated`, `SkipTooSmall`, `SkipTooLarge`,
`SkipLowSignal`, `SkipLanguageUnsupported`, and `SkipTestFixture`.

### Repo Memories

Repo memories are first-class local evidence, not chat memory. They are typed, source-anchored notes
bound to code or repository evidence.

Supported memory kinds:

- `Invariant`
- `Decision`
- `RejectedAlternative`
- `Risk`
- `BugPattern`
- `TestExpectation`
- `PerformanceNote`
- `SecurityNote`
- `FFIBoundary`
- `PlatformQuirk`
- `FollowUp`
- `OpenQuestion`
- `Obsolete`

Supported bindings:

- `logical_symbol_id`
- `symbol_id`
- `chunk_id`
- path plus optional line span
- graph `edge_id`
- call-path edge sequence hash
- commit hash
- GitHub issue/PR reference

Memories track `current`, `relocated`, `stale`, `gone`, or `unverified` anchor state. They surface
through `memory_*` tools and through integrated tools such as `read_chunk`, `symbol_lookup`,
`find_callers`, `trace_callees`, and `impact_surface`. Edge-bound memories appear under
`repo_memories.path_crossed` when an impact query crosses that graph edge.

Supported MCP tools:

- `memory_create`
- `memory_update`
- `memory_search`
- `memory_for_symbol`
- `memory_for_path`
- `memory_for_call_path`
- `memory_validate`
- `memory_mark_obsolete`

### Maintenance And Evaluation

Supported operational commands:

- `migrate` / `migrate --check`
- `doctor`
- `index_status`
- `heal_index`
- `hooks install/status/uninstall`
- `maintenance --trigger <hook> --max-seconds <n>`
- `eval`, `eval --json`, `eval --update-baseline`
- `dump-config`

`eval` runs fixture-driven ranking and freshness checks and reports search, graph, impact, git, and
papertrail metrics. Current-source violations must stay at zero.

## Known Limits

- Graph resolution is pragmatic tree-sitter analysis, not compiler/typechecker resolution.
- Kotlin graph extraction is useful but less mature than Rust and TypeScript.
- `index --watch` is reserved and currently returns an explicit not-implemented error.
- `semantic_search` is currently best understood as lexical BM25 recall plus freshness checks unless
  a real embedding model is installed and reconciled.
- `impact_surface` can return large JSON payloads; compact evidence views are still needed.
- FFI surface detection is heuristic.
- Call-path hash memories can be looked up, but authoritative edge-sequence hashes are not yet
  generated by traversal tools.
- Repo memories do not yet have review/approval workflow, multi-bind editing, or low-confidence
  filtering in integrated tools.

## Commands

Commands read `rag-rat.toml` by default. Use `--config <path>` when running from another directory
or with another repository profile.

```bash
cargo run --bin rag-rat -- index
cargo run --bin rag-rat -- index --changed
cargo run --bin rag-rat -- index --discover
cargo run --bin rag-rat -- index --full
cargo run --bin rag-rat -- doctor
cargo run --bin rag-rat -- migrate --check
cargo run --bin rag-rat -- migrate
cargo run --bin rag-rat -- github sync --from-refs
cargo run --bin rag-rat -- github sync --issue cq27-dev/rag-rat#42
cargo run --bin rag-rat -- github sync --from-refs --offline
cargo run --bin rag-rat -- models list
cargo run --bin rag-rat -- models install embedding-hash
cargo run --features fastembed --bin rag-rat -- models install fastembed-all-minilm-l6-v2
cargo run --bin rag-rat -- reconcile --limit 100 --batch-size 32
cargo run --bin rag-rat -- reconcile --changed-first --max-seconds 60 --batch-size 64
cargo run --bin rag-rat -- reconcile --until-clean --batch-size 64
cargo run --bin rag-rat -- hooks install
cargo run --bin rag-rat -- hooks status
cargo run --bin rag-rat -- maintenance --trigger post-checkout --max-seconds 30
cargo run --bin rag-rat -- eval
cargo run --bin rag-rat -- eval --json
cargo run --bin rag-rat -- eval --update-baseline
cargo run --bin rag-rat -- query "semantic recall"
cargo run --bin rag-rat -- mcp
```

By default, rag-rat links against the system SQLite library through `rusqlite`.

## MCP Server Install

The MCP server is a STDIO server, not an HTTP service. MCP clients start `rag-rat` as a child
process and talk to it over stdin/stdout.

For a reusable local install from this repo:

```bash
cargo install --path tools/rag-rat --bin rag-rat --features fastembed
rag-rat migrate --config /home/kk/src/held/rag-rat.toml
rag-rat index --discover --config /home/kk/src/held/rag-rat.toml
rag-rat models install fastembed-all-minilm-l6-v2 --config /home/kk/src/held/rag-rat.toml
rag-rat reconcile --changed-first --limit 500 --batch-size 64 --config /home/kk/src/held/rag-rat.toml
rag-rat doctor --config /home/kk/src/held/rag-rat.toml
```

Without real embeddings, omit `--features fastembed` and install `embedding-hash` instead:

```bash
cargo install --path tools/rag-rat --bin rag-rat
rag-rat models install embedding-hash --config /home/kk/src/held/rag-rat.toml
```

Add the installed binary to an MCP client config:

```json
{
  "mcpServers": {
    "rag-rat": {
      "command": "/home/kk/.cargo/bin/rag-rat",
      "args": ["mcp", "--config", "/home/kk/src/held/rag-rat.toml"]
    }
  }
}
```

For development without installing the binary, point the MCP client at Cargo:

```json
{
  "mcpServers": {
    "rag-rat-dev": {
      "command": "cargo",
      "args": [
        "run",
        "--manifest-path",
        "/home/kk/src/held/tools/rag-rat/Cargo.toml",
        "--features",
        "fastembed",
        "--bin",
        "rag-rat",
        "--",
        "mcp",
        "--config",
        "/home/kk/src/held/rag-rat.toml"
      ]
    }
  }
}
```

## Configuration

The host repo owns `rag-rat.toml`. This keeps monorepo-specific target bindings out of the reusable
tool.

```toml
[index]
root = "."
database = ".rag-rat/index.sqlite"

[local_ai.embedding.runtime]
batch_size = 64
ort_threads = 4
omp_threads = 1
max_embedding_chars = 4000

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

## Git Hooks

`rag-rat hooks install` installs generated `post-checkout`, `post-merge`, and `post-rewrite` hooks
for the current worktree. The hooks run in the background and call one bounded command:
`rag-rat maintenance --trigger <hook> --max-seconds 30`. Existing unmanaged hook files are never
overwritten.

`rag-rat maintenance` operates on the current worktree only. For branch switches, merges, and
rewrites it runs discover indexing for new/changed/deleted files, refreshes SQLite FTS through the
index path when needed, then reconciles embeddings with `changed_first` until the remaining time
budget is spent.

## Security

The MCP server exposes read-only source tools only. It does not execute shell commands or write
configured target files. It may write the configured SQLite index during indexing, migration,
maintenance, model reconciliation, repo-memory operations, and automatic stale-index healing before
returning search or `read_chunk` results.

GitHub sync is explicit and uses `gh api`; normal query tools read the local SQLite cache.

## License

`rag-rat` is licensed under the MIT License. See [LICENSE](LICENSE).

## Size Budget

Storage dependency changes must keep the binary slim. See `docs/binary-size.md` for the manual size
check and heavyweight dependency policy.
