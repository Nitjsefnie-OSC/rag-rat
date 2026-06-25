# Glama MCP verification image for cq27-dev/rag-rat.
#
# This is for Glama's hosted inspection harness, NOT general use — see the repo-root `Dockerfile`
# for a plain standalone image. Glama builds debian + node + mcp-proxy, clones the repo, and runs
# `mcp-proxy -- <command>`, which spawns the command as a stdio MCP server and exposes it at
# :8080/mcp (+/sse) with a /ping health check. Glama's stock template leaves <command> empty and
# ships no Rust toolchain, so this file adds both: a pinned toolchain + `cargo install`, and a real
# start command after `--`.
#
# The server roots itself in a tiny sample repo with embeddings off, so introspection
# (initialize + tools/list, served from a static catalog) boots instantly — no model download,
# no outbound network.
FROM debian:trixie-slim

ENV DEBIAN_FRONTEND=noninteractive \
    GLAMA_VERSION="1.0.0" \
    PYTHONUNBUFFERED=1 \
    RAG_RAT_NO_WATCH=1 \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH="/usr/local/cargo/bin:/app/node_modules/.bin:$PATH"

# node + mcp-proxy (Glama's harness); build-essential/pkg-config for rusqlite's bundled SQLite (cc);
# rustup toolchain pinned to satisfy rag-rat's 1.95+ MSRV.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git build-essential pkg-config \
    && curl -fsSL https://deb.nodesource.com/setup_26.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && npm install -g mcp-proxy@6.4.3 \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
         | sh -s -- -y --default-toolchain 1.96.0 --profile minimal \
    && apt-get clean && rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*

WORKDIR /app
RUN git clone https://github.com/cq27-dev/rag-rat . \
    && git checkout b903b9bebe8e9c194523ce48dca13bac659bcdd2

# Hash-only build (no FastEmbed) -> small and offline; identical MCP tool surface for introspection.
RUN cargo install --locked --path crates/rag-rat-cli --bin rag-rat \
        --no-default-features --root /usr/local

# Minimal sample repo the server roots itself in. Absolute paths so it's CWD-independent;
# model="none" (BM25-only) downloads nothing; watcher + crates.io version check disabled.
RUN mkdir -p /workspace/src \
    && printf '%s\n' \
        '[index]' 'root = "/workspace"' 'database = "/workspace/.rag-rat/index.sqlite"' '' \
        '[local_ai.embedding]' 'model = "none"' '' \
        '[watch]' 'enabled = false' '' \
        '[version_check]' 'enabled = false' '' \
        '[target_bindings]' 'rust = ["src"]' > /workspace/rag-rat.toml \
    && printf '%s\n' 'pub fn hello() {}' > /workspace/src/lib.rs \
    && git init -q /workspace
WORKDIR /workspace

# The fix vs. Glama's stock template: a non-empty start command after `--` (was `mcp-proxy -- ""`).
CMD ["mcp-proxy","--","rag-rat","--config","/workspace/rag-rat.toml","mcp"]
