# Index targets (`[target_bindings]` / `[[target]]`)

Part of the [config reference](../config.md).

Simple bindings map a language to directories:

```toml
[target_bindings]
rust = ["crates/app/src"]
typescript = ["apps/mobile/src"]
kotlin = ["apps/wear-bridge/src"]
cpp = ["include", "src"]
go = ["cmd", "internal"]
markdown = ["docs"]
```

A simple binding indexes each language's default extensions in the listed directories
(`rust` → `.rs`, `typescript` → `.ts`/`.tsx`, `python` → `.py`/`.pyi`, `c` → `.c`/`.h`,
`go` → `.go`, etc.).
The one ambiguous case is the `.h` header: with no binding it is detected as **C** (the safe
default), but an explicit `cpp` binding also claims `.h` in its directories and indexes those headers
as **C++**. This is what lets a C++ library whose API lives in `.h` files (most of them) get header
symbols, so calls resolve to their definitions instead of going unresolved. A `.c` file is never
treated as C++.

Expanded targets add name, kind, include, and exclude metadata:

```toml
[[target]]
name = "generated-bindings"
language = "typescript"
directories = ["packages/app/src/generated"]
kind = "generated"
include = ["**/*.ts"]
exclude = ["**/*.map"]
```

## `include` / `exclude` pattern syntax

Both lists are globs, matched against the `/`-separated repo-relative path. `exclude` is applied
first, so an excluded path is never claimed back by a broader `include`.

| Pattern | Matches |
| --- | --- |
| `**/*.ts` | that extension at any depth (this is what every default binding uses) |
| `*.ts` | that extension at the repo root only — one `*` does not cross a `/` |
| `src/**` | everything inside `src/`, and nothing whose name merely starts with `src` |
| `src/` | the same subtree; a trailing `/` is shorthand for `/**` |
| `src/*.ts` | direct children of `src/` only |
| `src/**/*.ts` | that extension anywhere under `src/` |
| `**/generated/**` | any `generated/` directory's contents, at any depth |
| `README.md` | that exact path, at the repo root — not `docs/README.md` |
| `{lib,main}.rs`, `a?c.rs`, `**/*.[ch]` | alternates, single-character wildcard, character class |

A pattern that is not a legal glob (an unclosed `[`, a dangling `\`) claims no files and is logged
at `warn`.

**Behaviour change.** These were previously matched by a small hand-written cascade that recognized
`**/*.ext` and `dir/**` and fell back to *substring containment* for everything else. Patterns of
those two shapes are unaffected — including every shipped default — but a pattern that relied on the
fallback now means what a glob says it means. Notably `*.rs` matched any path *containing* `.rs`
(claiming `notes.rs.bak` and `src/lib.rs.orig`) and now matches a root-level `.rs` file; a bare `*`
matched the whole tree and now matches root-level files only; and a literal such as `README.md` or
`vendor` matched any path containing it (`docs/README.md`, `x/vendor/dep.rs`) and now names one
path. A target `include` that relied on the old containment should be spelled with `**/`.

Supported languages are `rust`, `typescript`, `kotlin`, `c`, `cpp`, `python`, `swift`, `go`, and
`markdown`. Rust, TypeScript/TSX, Kotlin, C, C++, Python, Swift, and Go source use tree-sitter
structural indexing when files are under the parser size cap.
Markdown uses heading-section chunking and does not use tree-sitter. Supported target kinds are
`source`, `generated`, `docs`, and `tests`; generated targets are indexed with coarse chunks and
still obey `include_generated` filtering.

Parser grammar dependencies are exact-pinned in `Cargo.toml`: `tree-sitter` 0.26.11,
`tree-sitter-rust` 0.24.2, `tree-sitter-typescript` 0.23.2, `tree-sitter-kotlin-ng` 1.1.0,
`tree-sitter-c` 0.24.2, `tree-sitter-cpp` 0.23.4, `tree-sitter-python` 0.25.0,
`tree-sitter-swift` 0.7.3, and `tree-sitter-go` 0.25.0.
