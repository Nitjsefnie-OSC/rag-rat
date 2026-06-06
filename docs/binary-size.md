# Binary Size

`rag-rat` keeps storage dependencies small. SQLite is provided by the system through
`rusqlite`; heavyweight embedded database runtimes such as DuckDB are out of scope unless
a dedicated issue records the need, expected size impact, and accepted tradeoff.

Use this manual check after storage dependency changes:

```bash
CARGO_TARGET_DIR=/tmp/rag-rat-size-check RUSTC_WRAPPER= \
  cargo build --manifest-path Cargo.toml --bin rag-rat
du -h /tmp/rag-rat-size-check/debug/rag-rat
```

For a release-sized check:

```bash
CARGO_TARGET_DIR=/tmp/rag-rat-size-check RUSTC_WRAPPER= \
  cargo build --manifest-path Cargo.toml --bin rag-rat --release
du -h /tmp/rag-rat-size-check/release/rag-rat
```

Record the uncompressed binary size in the issue or pull request that changes storage
dependencies.
