# Minimal image for Glama's MCP server verification.
#
# Glama only needs the server to start and answer the `initialize` + `tools/list` introspection
# handshake. rag-rat serves `tools/list` from a static catalog, so it needs neither an index nor an
# embedding model to introspect — it only needs a `rag-rat.toml` to start. We therefore build the
# hash-only variant (`--no-default-features`) and configure `model = "none"`, so the image stays
# small and boots with no model download and no outbound network calls.
#
# NOTE: this is a verification image, not the distribution. The published binary
# (`cargo install rag-rat`) ships FastEmbed embeddings by default.

# ---- build stage ----
FROM rust:1.96-slim-bookworm AS builder

# `rusqlite` bundles SQLite and compiles it from source via `cc`, so a C toolchain is the only
# system build dependency.
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
# Only the workspace manifest and crates are needed — every path dependency lives under crates/
# and there are no build scripts that read other repo files.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo install --locked --path crates/rag-rat-cli --bin rag-rat \
        --no-default-features --root /usr/local

# ---- runtime stage ----
FROM debian:bookworm-slim

# ca-certificates for TLS; git lets the git/history tools work if Glama probes past introspection.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/rag-rat /usr/local/bin/rag-rat

# A minimal sample repo for the server to root itself in. `model = "none"` keeps embeddings off
# (BM25-only) so nothing is downloaded; the watcher and the crates.io version check are disabled so
# the container makes no outbound calls and boots instantly.
WORKDIR /workspace
RUN set -eux; \
    mkdir -p src; \
    printf '%s\n' \
      '[index]' \
      'root = "."' \
      'database = ".rag-rat/index.sqlite"' \
      '' \
      '[local_ai.embedding]' \
      'model = "none"' \
      '' \
      '[watch]' \
      'enabled = false' \
      '' \
      '[version_check]' \
      'enabled = false' \
      '' \
      '[target_bindings]' \
      'rust = ["src"]' \
      > rag-rat.toml; \
    printf '%s\n' 'pub fn hello() {}' > src/lib.rs; \
    git init -q .

# Belt-and-suspenders: also disable the file watcher via env.
ENV RAG_RAT_NO_WATCH=1

# Glama speaks MCP to the container over stdio.
CMD ["rag-rat", "mcp"]
