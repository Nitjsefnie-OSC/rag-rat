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
- `index_status`: `{}`

Search tools return chunk IDs, paths, line spans, short summaries, and scores. Search and `read_chunk` validate stored chunk anchors against current source before returning context; stale files are reindexed once per tool call and their SQLite FTS5 rows are updated before retrying. Use `read_chunk` only after a search or lookup has narrowed the context.

Auto-heal is capped at four files per call. If more stale files are detected, tools return a
`needs_reindex` error instead of doing unbounded work. Deleted source for a requested chunk returns
`Gone`; a chunk that disappears after file reindex returns `StaleChunk`.

`index_status` reports `content_revision`, `fts_source_revision`, `fts_dirty`, and `fts_fresh`.
Search tools synchronize dirty or stale SQLite FTS state before querying so results are not served
from an outdated FTS table.

Parser failures are visible through `index_status.parser_failure_paths`, with path, language, and
message for each failed source parse. Markdown files are chunked by headings instead of parsed with
tree-sitter.
