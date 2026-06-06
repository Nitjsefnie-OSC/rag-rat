# rag-rat

`rag-rat` is a local, read-only-source Rust repo-intelligence tool. It builds a SQLite FTS5 index over configured language roots and exposes CLI plus `rmcp` STDIO MCP access for LLM-assisted code search and propagation tracing.

Indexing uses tree-sitter structure for Rust, TypeScript/TSX, and Kotlin source where files are small enough for bounded parsing, markdown heading chunks for docs, and coarse chunks for generated or oversized files. Chunk anchors store content and context fingerprints so stale reads can be detected and repaired.

Tree-sitter grammars are exact-pinned in `Cargo.toml` so parser node coverage changes deliberately.

Chunk anchors include normalized text hashes, boundary hashes, nearby context hashes, and an anchor
version. `read_chunk` and search validate anchors against current source, relocate small line drift,
and cap automatic stale-file reindexing per call.

Git history is indexed into SQLite when the target root is a git worktree. Commit subjects/bodies
and path-level changes are historical evidence, separate from current source search. Chunk blame is
computed lazily for current chunk text and cached against the current source text hash.

## Commands

```bash
cargo run --bin rag-rat -- index --config rag-rat.toml
cargo run --bin rag-rat -- index --full --config rag-rat.toml
cargo run --bin rag-rat -- doctor --config rag-rat.toml
cargo run --bin rag-rat -- query --config rag-rat.toml "semantic recall"
cargo run --bin rag-rat -- mcp --config rag-rat.toml
```

By default, rag-rat links against the system SQLite library through `rusqlite`.

`index` updates files currently changed in git status. Use `index --full` to rebuild every file
and rebuild the SQLite FTS5 table from stored chunks.

Search never runs silently against stale FTS state. The index records a content revision for the
current `files`/`chunks` rows, tracks whether FTS is dirty after writes, and synchronizes FTS before
search when `fts_source_revision` no longer matches `content_revision`.

Non-git target roots still index source and docs; git history status reports unavailable with zero
commit/path rows.

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
