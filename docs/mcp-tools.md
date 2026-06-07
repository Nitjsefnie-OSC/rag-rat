# MCP Tools

`rag-rat mcp --config rag-rat.toml` starts a local `rmcp` STDIO server. The server is read-only for configured source roots; it may update the configured SQLite index when automatic stale-index healing is needed.

## Install

The MCP server is launched by the MCP client over STDIO. It does not listen on a TCP port.

Install the binary from a local checkout:

```bash
cargo install --path tools/rag-rat --bin rag-rat --features fastembed
rag-rat migrate --config /home/kk/src/held/rag-rat.toml
rag-rat index --discover --config /home/kk/src/held/rag-rat.toml
rag-rat models install fastembed-all-minilm-l6-v2 --config /home/kk/src/held/rag-rat.toml
rag-rat reconcile --limit 500 --config /home/kk/src/held/rag-rat.toml
rag-rat doctor --config /home/kk/src/held/rag-rat.toml
```

Use `embedding-hash` instead of FastEmbed when a small dependency footprint matters more than real
semantic embeddings:

```bash
cargo install --path tools/rag-rat --bin rag-rat
rag-rat models install embedding-hash --config /home/kk/src/held/rag-rat.toml
```

MCP client config for the installed binary:

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

Development config without installing:

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

## Tools

- `semantic_search`: `{ "query": string, "limit"?: number, "include_generated"?: boolean, "include_graph"?: "none" | "compact" | "full", "graph_limit"?: number, "include_git"?: boolean, "include_papertrail"?: boolean, "explain"?: boolean }`
- `symbol_lookup`: `{ "symbol"?: string, "symbol_path"?: string, "symbol_id"?: number, "language"?: string, "allow_ambiguous"?: boolean, "limit"?: number }`
- `find_callers`: `{ "symbol"?: string, "symbol_path"?: string, "symbol_id"?: number, "resolution"?: "exact" | "syntactic" | "fuzzy", "allow_ambiguous"?: boolean, "limit"?: number, "include_references"?: boolean, "edge_kinds"?: string[] }`
- `trace_callees`: `{ "symbol"?: string, "symbol_path"?: string, "symbol_id"?: number, "resolution"?: "exact" | "syntactic" | "fuzzy", "allow_ambiguous"?: boolean, "limit"?: number, "include_references"?: boolean, "edge_kinds"?: string[] }`
- `impact_surface`: `{ "query"?: string, "symbol"?: string, "symbol_path"?: string, "symbol_id"?: number, "resolution"?: "exact" | "syntactic" | "fuzzy", "allow_ambiguous"?: boolean, "limit"?: number }`
- `ffi_surface`: `{ "limit"?: number }`
- `docs_for_symbol`: `{ "symbol"?: string, "symbol_path"?: string, "symbol_id"?: number, "allow_ambiguous"?: boolean, "limit"?: number }`
- `read_chunk`: `{ "chunk_id": number, "include_graph"?: "none" | "compact" | "full", "graph_limit"?: number }`
- `commit_search`: `{ "query": string, "limit"?: number }`
- `git_history_for_path`: `{ "path": string, "limit"?: number }`
- `git_history_for_symbol`: `{ "symbol"?: string, "symbol_path"?: string, "symbol_id"?: number, "language"?: string, "allow_ambiguous"?: boolean, "limit"?: number }`
- `commits_touching_query`: `{ "query": string, "limit"?: number }`
- `git_blame_chunk`: `{ "chunk_id": number }`
- `papertrail_for_chunk`: `{ "chunk_id": number, "limit"?: number }`
- `papertrail_for_symbol`: `{ "symbol"?: string, "symbol_path"?: string, "symbol_id"?: number, "language"?: string, "allow_ambiguous"?: boolean, "limit"?: number }`
- `papertrail_for_commit`: `{ "commit_hash": string, "limit"?: number }`
- `github_issue_search`: `{ "query": string, "limit"?: number }`
- `github_refs_for_path`: `{ "path": string, "limit"?: number }`
- `rationale_search`: `{ "query": string, "limit"?: number }`
- `local_ai_status`: `{}`
- `heal_index`: `{ "limit"?: number }`
- `github_sync_status`: `{}`
- `index_status`: `{}`

`tools/list` is served by `rmcp` and exposes typed JSON schemas derived from the same request structs
used by the handlers. Existing tool names and response fields are kept stable for current MCP clients.

`symbol_lookup` is the candidate-selection step for symbol-shaped tools. It returns:

```json
{
  "candidates": [
    {
      "symbol_id": 3150,
      "symbol_path": "core/held-core/src/runtime/task_spawn.rs::spawn_blocking",
      "qualified_name": "core/held-core/src/runtime/task_spawn.rs::spawn_blocking",
      "kind": "function",
      "signature": "pub(crate) async fn spawn_blocking<F, T>(...)"
    }
  ],
  "disambiguation_required": true
}
```

Graph, docs, git-history, papertrail, and symbol-specific impact tools accept `symbol_id`,
`symbol_path`, or `symbol` and resolve them in that priority order. If a bare `symbol` maps to
multiple candidates, the tool returns the same candidate object instead of guessing. Pass
`allow_ambiguous: true` only for navigation/debugging when name fallback is acceptable.

Search tools return chunk IDs, paths, line spans, current-text snippets in the `summary` field, and
scores. Search and `read_chunk` validate stored chunk anchors against current source before
returning context; stale files are reindexed once per tool call and their SQLite FTS5 rows are
updated before retrying. Use `read_chunk` only after a search or lookup has narrowed the context.

Auto-heal is capped at four files per call. If more stale files are detected, tools return a
`needs_reindex` error instead of doing unbounded work. Deleted source for a requested chunk returns
`Gone`; a chunk that disappears after file reindex returns `StaleChunk`.

`index_status` reports `content_revision`, `fts_source_revision`, `fts_dirty`, and `fts_fresh`.
Search tools synchronize dirty or stale SQLite FTS state before querying so results are not served
from an outdated FTS table.

`semantic_search` graph and evidence controls:

- `include_graph`: `none`, `compact`, or `full`. Default is `compact`.
- `graph_limit`: maximum caller/callee/import/type evidence entries to attach. Default is `3` for
  search and `20` for `read_chunk`.
- `include_git`: include git-history ranking boosts when available. Default is `true`.
- `include_papertrail`: include cached GitHub papertrail ranking boosts when available. Default is
  `true`.
- `explain`: include score components (`bm25`, `vector`, `symbol`, `graph`, `git`, `github`).
  Default is `false`.

Graph tools are backed by tree-sitter-derived syntax edges. Edge kinds are `imports`, `exports`,
`calls_name`, `constructs`, `uses_macro`, `references_type`, `implements`, and `contains`; confidence is reported as
`edge_confidence` (`confidence` is retained as the compatibility alias) with values `Exact`,
`Syntactic`, `NameOnly`, or `Ambiguous`. Graph evidence is syntactic, confidence-labeled evidence,
not compiler-grade name resolution or hard truth. Search results default to compact graph evidence
with bounded caller/callee lists; `read_chunk` defaults to full graph evidence. Caller and callee
entries include exact tree-sitter callsite spans: `callsite.path`, `callsite.line`, and
`callsite.span` (`[start_line, end_line]`).

Graph tools and `impact_surface` accept `resolution`:

- `exact`: only verified target-symbol rows are returned. A row is allowed only when
  `target_symbol_id` matches `symbol_id`, or the resolved fully-qualified symbol identity matches
  `symbol`. Every returned row has `verified_target_symbol: true`.
- `syntactic`: default. Exact matches plus qualified syntactic evidence are returned. Unresolved
  qualified call targets may be shown, but broad bare-name ambiguous fallback is excluded.
- `fuzzy`: compatibility/navigation mode. Suffix and bare-name fallback are allowed, including
  ambiguous candidates. Treat these rows as possible evidence, not proof.

Use `symbol_id` with `resolution: "exact"` when a previous `symbol_lookup` result selected one
specific symbol. Bare names in `exact` mode intentionally return little or nothing unless
`symbol_id` is provided.

`find_callers` and `trace_callees` return a graph envelope, not a bare row array:

```json
{
  "query": {
    "tool": "find_callers",
    "symbol_id": 3150,
    "symbol_path": "core/held-core/src/runtime/task_spawn.rs::spawn_blocking",
    "resolution": "exact"
  },
  "summary": {
    "returned_count": 38,
    "total_matching_edges": 38,
    "truncated": false,
    "exact_verified": 38,
    "syntactic": 0,
    "name_only": 0,
    "ambiguous": 0,
    "unresolved": 0,
    "false_positive_risk": "low"
  },
  "coverage": {
    "indexed_files": 1055,
    "parser_failures": 0,
    "source_stale_files": 0,
    "known_index_gaps": [],
    "parser_coverage_for_paths": []
  },
  "results": []
}
```

The trust signal is in `summary` and `coverage`: exact mode should have
`exact_verified == total_matching_edges`, `truncated: false`, no parser failures for covered paths,
and `source_stale_files: 0`. If the source changed after indexing, `source_stale_files` is non-zero
for the covered paths and the result should be treated as needing reindex before audit use.

`trace_callees` is call-only by default: it returns `calls_name` and `constructs` edges. Type
references, imports, exports, `contains`, `implements`, and macros are excluded unless
`include_references` is true or `edge_kinds` is provided explicitly. `find_callers` uses exact
target resolution first, then qualified-name and target-name fallbacks; fallback hits are labeled
with `resolution`, `verified_target_symbol`, raw `evidence`, and optional `receiver_hint` so clients
can treat name-only or ambiguous edges as possible evidence instead of compiler truth. Rust macro
invocations are stored as `uses_macro` and are not resolved to same-named normal modules or
functions.

`impact_surface` is graph-backed first and defaults to `resolution: "syntactic"`. It layers exact
symbol definitions, direct callers, direct callees, import/export dependents, same-file siblings,
git commits touching those files, and cached GitHub papertrail. Pass `resolution: "fuzzy"` to include
possible name-only graph evidence; those rows remain labeled by confidence and should be treated as
possible impact. Text/path LIKE matching is used only as `Probable textual impact` fallback. Each
item includes `category`, `reason`, and `evidence`; categories are `Direct structural impact`,
`Probable textual impact`, and `Historical/papertrail evidence`.

Git history tools return historical evidence. `commit_search` searches commit subjects and bodies;
`git_history_for_path` returns commits touching a current path; `git_history_for_symbol` resolves
the current symbol path/range first, then returns path history; `commits_touching_query` combines
commit-message evidence with current file-change evidence; `git_blame_chunk` computes blame lazily
and caches it by `source_text_hash`.

`index_status.git_history` reports whether git history is available, current git HEAD, indexed HEAD,
commit count, and file-change count. Non-git roots report unavailable history without failing source
or docs indexing.

GitHub papertrail tools read the local cache only. `rag-rat github sync --from-refs` discovers refs
and fetches through `gh api --paginate`; `rag-rat github sync --issue owner/repo#123` fetches one
issue/PR thread; `--offline` updates discovered refs and reports cache status without network use.

Papertrail outputs keep `current_source` separate from `github_evidence`. GitHub snippets are labeled
as historical GitHub evidence and classified as `decision`, `rejected_alternative`, `constraint`,
`risk`, `obsolete`, or `context`.

`index_status.github` reports cached refs, issues, comments, pulls, reviews, review comments,
last sync time, and whether the `gh` CLI capability is available.
`github_sync_status` returns that GitHub cache section directly. `heal_index` repairs or removes
already-indexed files whose current source no longer matches the stored SQLite index, then refreshes
SQLite FTS. It does not discover brand-new files; run `rag-rat index` for discovery.

Local AI artifacts are explicit and current-only. `local_ai_status` reports embedding model state
and artifact counts. The CLI-only `models install embedding-hash` command selects the deterministic
baseline embedder. Building with `--features fastembed` enables the real local
`fastembed-all-minilm-l6-v2` backend; `models install fastembed-all-minilm-l6-v2` is the intended
FastEmbed cache-population step. `doctor` reports FastEmbed build support, cache path, model,
dimension, current/stale/missing/failed embedding counts, and the next command needed to make local
AI current. `reconcile` writes model-id, dimension, and text-hash-bound chunk embeddings in
configurable batches for current chunk hashes only.
`semantic_search` combines BM25 candidates, vector similarity, symbol/name/path boosts,
graph-neighborhood boosts, and optional git/GitHub papertrail boosts. Embeddings are used only when
the active model is installed, the embedding dimension matches active model metadata, the artifact
status is `Current`, and the artifact text hash matches the current chunk text hash; stale
embeddings are treated as absent. There is no summarizer or LLM runtime in this milestone.

Indexing and reconcile use a Rayon worker pool for CPU-heavy preparation: file reads, hashing,
tree-sitter chunk/symbol preparation, git-log parsing, and embedding computation run across available
cores. SQLite writes are still serialized through the local index connection and transaction-batched
where the command owns the write scope.

Parser failures are visible through `index_status.parser_failure_paths`, with path, language, and
message for each failed source parse. Markdown files are chunked by headings instead of parsed with
tree-sitter.
