# rag-rat

`rag-rat` is a local, read-only-source Rust repo-intelligence tool. It builds a DuckDB FTS index over configured language roots and exposes CLI plus `rmcp` STDIO MCP access for LLM-assisted code search and propagation tracing.

Indexing uses tree-sitter structure for Rust, TypeScript/TSX, and Kotlin source where files are small enough for bounded parsing, markdown heading chunks for docs, and coarse chunks for generated or oversized files. Chunk anchors store content and context fingerprints so stale reads can be detected and repaired.

## Commands

```bash
cargo run --bin rag-rat -- index --config rag-rat.toml
cargo run --bin rag-rat -- doctor --config rag-rat.toml
cargo run --bin rag-rat -- query --config rag-rat.toml "semantic recall"
cargo run --bin rag-rat -- mcp --config rag-rat.toml
```

## Configuration

The host repo owns `rag-rat.toml`. This keeps monorepo-specific target bindings out of the reusable tool.

```toml
[index]
root = "."
database = ".rag-rat/index.duckdb"

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

The MCP server exposes read-only source tools only. It does not execute shell commands or write configured target files. It may write the configured DuckDB index during `index` and during automatic stale-index healing before returning search or `read_chunk` results.
