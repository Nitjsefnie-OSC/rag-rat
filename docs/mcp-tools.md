# MCP Tools

`rag-rat mcp --config rag-rat.toml` starts a local `rmcp` STDIO server. The server is read-only for configured source roots; it may update the configured SQLite index when automatic stale-index healing is needed.

## Tools

- `semantic_search`: `{ "query": string, "limit"?: number, "include_generated"?: boolean }`
- `symbol_lookup`: `{ "symbol": string, "language"?: string, "limit"?: number }`
- `find_callers`: `{ "symbol": string, "limit"?: number }`
- `trace_callees`: `{ "symbol": string, "limit"?: number }`
- `impact_surface`: `{ "query": string, "limit"?: number }`
- `ffi_surface`: `{ "limit"?: number }`
- `docs_for_symbol`: `{ "symbol": string, "limit"?: number }`
- `read_chunk`: `{ "chunk_id": number }`
- `commit_search`: `{ "query": string, "limit"?: number }`
- `git_history_for_path`: `{ "path": string, "limit"?: number }`
- `git_history_for_symbol`: `{ "symbol": string, "language"?: string, "limit"?: number }`
- `commits_touching_query`: `{ "query": string, "limit"?: number }`
- `git_blame_chunk`: `{ "chunk_id": number }`
- `papertrail_for_chunk`: `{ "chunk_id": number, "limit"?: number }`
- `papertrail_for_symbol`: `{ "symbol": string, "language"?: string, "limit"?: number }`
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

Search tools return chunk IDs, paths, line spans, short summaries, and scores. Search and `read_chunk` validate stored chunk anchors against current source before returning context; stale files are reindexed once per tool call and their SQLite FTS5 rows are updated before retrying. Use `read_chunk` only after a search or lookup has narrowed the context.

Auto-heal is capped at four files per call. If more stale files are detected, tools return a
`needs_reindex` error instead of doing unbounded work. Deleted source for a requested chunk returns
`Gone`; a chunk that disappears after file reindex returns `StaleChunk`.

`index_status` reports `content_revision`, `fts_source_revision`, `fts_dirty`, and `fts_fresh`.
Search tools synchronize dirty or stale SQLite FTS state before querying so results are not served
from an outdated FTS table.

Graph tools are backed by tree-sitter-derived syntax edges. Edge kinds are `imports`, `exports`,
`calls_name`, `references_type`, `implements`, and `contains`; confidence is reported as `Exact`,
`Syntactic`, `NameOnly`, or `Ambiguous`. These are intentionally not compiler-grade resolved call
graphs.

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

Local AI artifacts are explicit and current-only. MCP query paths never install or download models.
`local_ai_status` reports model capability state, artifact counts, and whether embeddings or summaries
are ready, missing, stale, blocked, disabled, or failed. The CLI-only `models install <model-id>`
command records explicit local model availability, and `reconcile` writes embeddings and summaries for
current chunk hashes only. Hybrid search degrades to lexical/structural evidence when a model is
missing or an artifact hash does not match the current chunk text; stale summaries and embeddings are
treated as absent.

Parser failures are visible through `index_status.parser_failure_paths`, with path, language, and
message for each failed source parse. Markdown files are chunked by headings instead of parsed with
tree-sitter.
