# Config Reference

`rag-rat.toml` has an `[index]` table, optional simple `[target_bindings]`, and optional richer `[[target]]` blocks.

```toml
[index]
root = "."
database = ".rag-rat/index.sqlite"
```

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

Supported languages are `rust`, `typescript`, `kotlin`, and `markdown`. Rust, TypeScript/TSX, and Kotlin source use tree-sitter structural indexing when files are under the parser size cap. Supported target kinds are `source`, `generated`, `docs`, and `tests`; generated targets are indexed with coarse chunks and still obey `include_generated` filtering.
