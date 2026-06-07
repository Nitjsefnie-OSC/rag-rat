# Config Reference

`rag-rat.toml` has an `[index]` table, optional simple `[target_bindings]`, and optional richer `[[target]]` blocks.

```toml
[index]
root = "."
database = ".rag-rat/index.sqlite"

[local_ai.embedding.runtime]
batch_size = 64
ort_threads = 4
omp_threads = 1
max_embedding_chars = 4000
```

The database stores explicit schema migrations in `schema_version` with migration id,
`applied_at_ms`, checksum, and description. `rag-rat migrate --check` verifies compatibility without
changing source files; `rag-rat migrate` applies the current SQLite index schema. Normal runtime
opens refuse older, newer, dirty, or partial schemas instead of silently altering tables.

Simple bindings map a language to directories:

```toml
[target_bindings]
rust = ["core/held-core/src"]
typescript = ["apps/mobile/src"]
kotlin = ["apps/wear-bridge/src"]
markdown = ["docs"]
```

Expanded targets add name, kind, include, and exclude metadata:

```toml
[[target]]
name = "held-core-generated-bindings"
language = "typescript"
directories = ["packages/held-core/src/generated"]
kind = "generated"
include = ["**/*.ts"]
exclude = ["**/*.map"]
```

Supported languages are `rust`, `typescript`, `kotlin`, and `markdown`. Rust, TypeScript/TSX,
and Kotlin source use tree-sitter structural indexing when files are under the parser size cap.
Markdown uses heading-section chunking and does not use tree-sitter. Supported target kinds are
`source`, `generated`, `docs`, and `tests`; generated targets are indexed with coarse chunks and
still obey `include_generated` filtering.

Parser grammar dependencies are exact-pinned in `Cargo.toml`: `tree-sitter` 0.22.6,
`tree-sitter-rust` 0.21.2, `tree-sitter-typescript` 0.21.2, and `tree-sitter-kotlin` 0.3.8.

`[local_ai.embedding.runtime]` controls reconcile defaults for local embedding generation. CLI flags
still take precedence: `--batch-size` overrides `batch_size`, and `--max-embedding-chars` overrides
`max_embedding_chars`. `ort_threads` and `omp_threads` are applied as `ORT_NUM_THREADS` and
`OMP_NUM_THREADS` when those environment variables are not already set by the caller.

`rag-rat hooks install` writes generated `post-checkout`, `post-merge`, and `post-rewrite` hooks to
the current worktree's Git hooks directory. Those hooks call `rag-rat maintenance --max-seconds 30`
in the background so branch switches, merges, and rebases refresh the current worktree index and
advance changed-first embedding reconciliation without blocking normal Git operations.
