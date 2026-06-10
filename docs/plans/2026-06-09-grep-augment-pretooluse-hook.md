# Grep-Augmentation PreToolUse Hook Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Claude Code PreToolUse hook that augments Grep/Bash-grep tool calls with rag-rat symbol + memory context, served by the running MCP server over a Unix socket with a read-only SQLite fallback.

**Architecture:** Three components per the spec (`docs/specs/2026-06-09-grep-augment-pretooluse-hook.md`): (1) `rag-rat claude-hook` CLI dispatch that reads the PreToolUse JSON from stdin and prints `additionalContext` JSON; (2) a Unix-socket listener inside `rag-rat mcp`, owned via a second election lock, holding per-session dedupe state in memory; (3) shared payload composition in `rag-rat-core::query::grep_augment`, reused by a stateless direct-SQLite fallback. Never blocks, never loads the embedding model.

**Tech Stack:** Rust 2024, rusqlite, tokio (`UnixListener`), serde_json, existing `locks::FileLock` election primitives. No new external dependencies.

**Worktree note:** Execute in a separate worktree. The spec and this plan are deliberately **uncommitted** (user convention); copy both files into the worktree before starting (`cp docs/specs/2026-06-09-grep-augment-pretooluse-hook.md docs/plans/2026-06-09-grep-augment-pretooluse-hook.md <worktree>/docs/...`), and never `git add` them. Always `git add` explicit paths, never `-A`.

**Style note:** Follow the repo-local `rust-modern-style` skill (invoke it before coding): `{self, ..}` imports for mixed lists, domain-question-named SQL helpers with invariant comments, injected time where applicable, `mod.rs` as curated index.

**Verification gate per task:** `cargo test -p <crate> <filter>` as given, plus `cargo clippy --all-targets` and `cargo fmt` before each commit.

---

### Task 1: `locks` — worktree hash extraction, socket lock path, socket path

**Files:**
- Modify: `crates/rag-rat-core/src/locks.rs`

The election lock keys on a sha256 of the canonicalized worktree root (`election_lock_path`, `locks.rs:92`). Extract that hashing into a helper and add two siblings: the socket-election lock path and the socket path itself (with `sun_path`-length fallback).

- [ ] **Step 1: Write the failing tests** (append to `mod tests` in `locks.rs`)

```rust
#[test]
fn socket_lock_path_is_distinct_from_election_lock_path() {
    let base = temp_dir();
    let root = temp_dir();
    let election = election_lock_path(&base, &root);
    let socket_lock = socket_lock_path(&base, &root);
    assert_ne!(election, socket_lock);
    assert!(socket_lock.to_string_lossy().ends_with(".socket.lock"));
    // Same worktree key: both live under <base>/locks/ with the same hash stem.
    assert_eq!(election.parent(), socket_lock.parent());
}

#[test]
fn hook_socket_path_lives_under_base_sockets_dir() {
    let base = temp_dir();
    let root = temp_dir();
    let socket = hook_socket_path(&base, &root);
    assert_eq!(socket.parent().unwrap().file_name().unwrap(), "sockets");
    assert!(socket.extension().is_some_and(|ext| ext == "sock"));
}

#[test]
fn hook_socket_path_falls_back_when_base_path_is_too_long() {
    // sun_path is ~108 bytes; a deeply nested base dir must divert to a short runtime dir.
    let mut long_base = temp_dir();
    for _ in 0..12 {
        long_base.push("very-long-directory-segment");
    }
    let root = temp_dir();
    let socket = hook_socket_path(&long_base, &root);
    assert!(
        socket.as_os_str().len() <= MAX_SOCKET_PATH_LEN,
        "fallback path still too long: {}",
        socket.display()
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p rag-rat-core locks:: -- --nocapture`
Expected: FAIL — `socket_lock_path`, `hook_socket_path`, `MAX_SOCKET_PATH_LEN` not found.

- [ ] **Step 3: Implement**

In `locks.rs`, refactor `election_lock_path` to use a new private helper and add the two public fns:

```rust
/// `sun_path` budget for Unix domain sockets (108 bytes on Linux, 104 on macOS) with headroom.
pub const MAX_SOCKET_PATH_LEN: usize = 100;

/// Stable per-worktree key: sha256 of the canonicalized root (see `election_lock_path` doc
/// comment for why canonicalize-but-not-case-fold).
fn worktree_hash(worktree_root: &Path) -> String {
    let canonical = worktree_root.canonicalize().unwrap_or_else(|_| worktree_root.to_path_buf());
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    let mut hash = String::with_capacity(32);
    for byte in &digest[..16] {
        use std::fmt::Write as _;
        let _ = write!(hash, "{byte:02x}");
    }
    hash
}

/// Election lock for the grep-augment hook socket: one listener per worktree, separate from the
/// watcher election so core never calls back into the MCP crate and either process may win each.
pub fn socket_lock_path(base_dir: &Path, worktree_root: &Path) -> PathBuf {
    base_dir.join("locks").join(format!("{}.socket.lock", worktree_hash(worktree_root)))
}

/// Where the elected listener binds. Prefers a `sockets/` sibling of `locks/` under the shared
/// DB dir; diverts to `$XDG_RUNTIME_DIR/rag-rat/` then the OS temp dir when the result would
/// exceed the `sun_path` budget. Hook clients compute the same path independently, so this must
/// stay deterministic for a given (base_dir, worktree_root) and environment.
pub fn hook_socket_path(base_dir: &Path, worktree_root: &Path) -> PathBuf {
    let name = format!("{}.sock", worktree_hash(worktree_root));
    let preferred = base_dir.join("sockets").join(&name);
    if preferred.as_os_str().len() <= MAX_SOCKET_PATH_LEN {
        return preferred;
    }
    let runtime_base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    runtime_base.join("rag-rat").join(name)
}
```

Rewrite `election_lock_path` body to `base_dir.join("locks").join(format!("{}.lock", worktree_hash(worktree_root)))` (behavior identical — same digest, same formatting).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rag-rat-core locks::`
Expected: PASS (including pre-existing lock tests — the refactor must not change election paths).

- [ ] **Step 5: Commit**

```bash
git add crates/rag-rat-core/src/locks.rs
git commit -m "feat(locks): socket election lock + hook socket path helpers"
```

---

### Task 2: `storage` — read-only open for the hook fallback

**Files:**
- Modify: `crates/rag-rat-core/src/storage.rs`

- [ ] **Step 1: Write the failing test** (append to storage.rs tests, or create `mod tests` mirroring `locks.rs` style if absent)

```rust
#[test]
fn open_read_only_reads_but_rejects_writes() {
    let dir = std::env::temp_dir().join(format!("ragrat-ro-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("index.db");
    {
        let rw = IndexConnection::open(&db).unwrap();
        crate::index::schema::apply(rw.connection()).unwrap();
    }
    let ro = IndexConnection::open_read_only(&db).unwrap();
    let n: i64 =
        ro.connection().query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0)).unwrap();
    assert_eq!(n, 0);
    let err = ro.connection().execute("INSERT INTO index_meta(key, value) VALUES('x','y')", []);
    assert!(err.is_err(), "read-only connection must reject writes");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn open_read_only_fails_cleanly_when_database_missing() {
    let missing = std::env::temp_dir().join("ragrat-ro-missing/never-created.db");
    assert!(IndexConnection::open_read_only(&missing).is_err());
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p rag-rat-core storage::`
Expected: FAIL — `open_read_only` not found.

- [ ] **Step 3: Implement** (in `impl IndexConnection`, after `open`)

```rust
/// Read-only open for latency-critical, never-blocking callers (the grep-augment hook
/// fallback). Skips `setup()` — no pragma writes, no dir creation — and refuses to create
/// the file. WAL databases serve concurrent read-only opens; a DB that has never been
/// opened for write errors here, which callers treat as "no context".
pub fn open_read_only(path: &Path) -> anyhow::Result<Self> {
    use rusqlite::OpenFlags;
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_millis(100))?;
    Ok(Self { conn, database_path: path.to_path_buf(), source_root: None })
}
```

- [ ] **Step 4: Run tests** — `cargo test -p rag-rat-core storage::` Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/rag-rat-core/src/storage.rs
git commit -m "feat(storage): read-only IndexConnection open for hook fallback"
```

---

### Task 3: `search::lexical` — FTS-only search entry point

**Files:**
- Modify: `crates/rag-rat-core/src/search/lexical.rs`

`search_with_options` (`lexical.rs:107`) calls `ai::embed_query` — that can load the embedding model, which the hook must never do. The private `search_with_query_embedding` already accepts `None`; expose a wrapper.

- [ ] **Step 1: Write the failing test** (new `mod tests` at the bottom of `lexical.rs` if absent; use `schema::apply` on an in-memory connection)

```rust
#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::index::schema;

    fn seeded_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
             VALUES ('src/watch.rs', 'rust', 'source', 'abc', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte,
                                start_line, end_line, text, text_hash)
             VALUES (1, 'symbol', 'watcher_main', 0, 10, 1, 20,
                     'fn watcher_main() { /* election retry loop */ }', 'h1')",
            [],
        )
        .unwrap();
        schema::rebuild_fts(&conn).unwrap();
        conn
    }

    #[test]
    fn search_lexical_only_returns_bm25_hits_without_embeddings() {
        let conn = seeded_conn();
        let hits = search_lexical_only(&conn, "election retry", 5, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "src/watch.rs");
        // No model is configured in this DB; reaching here without error proves no embed path ran.
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p rag-rat-core search_lexical_only`
Expected: FAIL — function not found. (If the seed INSERTs fail on NOT NULL columns, fix the seed to match `index/schema.rs:230-266` — the canonical column lists are there.)

- [ ] **Step 3: Implement** (next to `search_with_options`)

```rust
/// BM25/FTS-only search for latency-critical callers (the grep-augment hook): bypasses
/// `ai::embed_query`, so it can never trigger an embedding-model load. Also skips git and
/// papertrail boosts — pure lexical + structural rank.
pub fn search_lexical_only(
    conn: &Connection,
    query: &str,
    limit: u32,
    include_generated: bool,
) -> anyhow::Result<Vec<SearchHit>> {
    search_with_query_embedding(
        conn,
        query,
        limit,
        include_generated,
        None,
        false,
        SearchOptions { include_git: false, include_papertrail: false },
    )
}
```

- [ ] **Step 4: Run tests** — `cargo test -p rag-rat-core search_lexical_only` Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/rag-rat-core/src/search/lexical.rs
git commit -m "feat(search): FTS-only lexical entry point (no embedding load)"
```

---

### Task 4: `query::grep_augment` — pattern normalization + identifier detection

**Files:**
- Create: `crates/rag-rat-core/src/query/grep_augment.rs`
- Modify: `crates/rag-rat-core/src/query/mod.rs` (add `pub mod grep_augment;` to the index, alphabetical order)

- [ ] **Step 1: Write the failing tests** (in the new file, code-under-test above them)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_regex_metacharacters_and_anchors() {
        assert_eq!(normalize_pattern(r"^fn\s+watcher_main\b"), "fn watcher_main");
        assert_eq!(normalize_pattern(r"Watcher::spawn(_with_fleet)?"), "Watcher::spawn _with_fleet");
        assert_eq!(normalize_pattern("plain words"), "plain words");
        assert_eq!(normalize_pattern(r".*[]()|+?^$\\"), "");
    }

    #[test]
    fn identifier_candidate_accepts_identifier_shapes_only() {
        assert_eq!(identifier_candidate("watcher_main"), Some("watcher_main"));
        assert_eq!(identifier_candidate("Watcher::spawn"), Some("Watcher::spawn"));
        assert_eq!(identifier_candidate("foo.bar"), Some("foo.bar"));
        assert_eq!(identifier_candidate("fn watcher_main"), None); // two words
        assert_eq!(identifier_candidate("ab"), None); // too short
        assert_eq!(identifier_candidate("1abc"), None); // leading digit
        assert_eq!(identifier_candidate(""), None);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p rag-rat-core grep_augment::`
Expected: FAIL to compile — functions not defined.

- [ ] **Step 3: Implement**

```rust
//! Payload composition for the Claude Code grep-augmentation PreToolUse hook.
//!
//! Shared by the `rag-rat mcp` socket listener (with per-session dedupe) and the hook
//! client's direct read-only fallback (stateless). Spec:
//! `docs/specs/2026-06-09-grep-augment-pretooluse-hook.md`. Never loads the embedding
//! model — symbol/FTS lanes only.

/// Strip regex syntax from a grep pattern, leaving plain query text. Metacharacters become
/// spaces (so alternation/group contents survive as separate words); runs of whitespace
/// collapse; result is trimmed.
pub fn normalize_pattern(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                // Drop the escape and its class letter (\s, \b, \w...); keep escaped literals.
                if let Some(&next) = chars.peek() {
                    chars.next();
                    if !next.is_ascii_alphanumeric() {
                        out.push(next);
                    } else {
                        out.push(' ');
                    }
                }
            },
            '^' | '$' | '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' => {
                out.push(' ');
            },
            _ => out.push(ch),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A normalized pattern that looks like one code identifier (optionally `::`/`.`-qualified):
/// the symbol-lane trigger. Multi-word or short patterns return `None`.
pub fn identifier_candidate(normalized: &str) -> Option<&str> {
    if normalized.len() < 3 || normalized.contains(' ') {
        return None;
    }
    let mut chars = normalized.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.')).then_some(normalized)
}
```

- [ ] **Step 4: Run tests** — `cargo test -p rag-rat-core grep_augment::` Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/rag-rat-core/src/query/grep_augment.rs crates/rag-rat-core/src/query/mod.rs
git commit -m "feat(query): grep_augment pattern normalization + identifier detection"
```

---

### Task 5: `query::grep_augment` — compose + render with dedupe filter

**Files:**
- Modify: `crates/rag-rat-core/src/query/grep_augment.rs`

- [ ] **Step 1: Write the failing tests** (extend `mod tests`; seeding uses `schema::apply` + raw inserts + `create_memory`, mirroring Task 3's pattern)

```rust
use std::collections::HashSet;

use rusqlite::Connection;

use crate::index::schema;
use crate::query::memory::{self, RepoMemoryBindTarget, RepoMemoryCreate};

fn seeded_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
         VALUES ('src/watch.rs', 'rust', 'source', 'abc', 0, 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name, kind, start_byte,
                             end_byte, signature, docs)
         VALUES (1, 'rust', 'watcher_main', 'watch::watcher_main', 'function', 0, 100,
                 'fn watcher_main(config: Config)', NULL)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte,
                            start_line, end_line, text, text_hash)
         VALUES (1, 'symbol', 'watcher_main', 0, 100, 1, 20,
                 'fn watcher_main() { /* election retry loop */ }', 'h1')",
        [],
    )
    .unwrap();
    // One caller edge and one callee edge for the counts line.
    conn.execute(
        "INSERT INTO edges(source_file_id, from_symbol_id, to_symbol_id, to_name,
                           target_qualified_name, edge_kind, confidence)
         VALUES (1, NULL, 1, 'watcher_main', 'watch::watcher_main', 'calls_name', 'exact')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO edges(source_file_id, from_symbol_id, to_symbol_id, to_name,
                           target_qualified_name, edge_kind, confidence)
         VALUES (1, 1, NULL, 'maintenance_pass', NULL, 'calls_name', 'name_only')",
        [],
    )
    .unwrap();
    memory::create_memory(
        &conn,
        RepoMemoryCreate {
            kind: "invariant".to_string(),
            title: "One watcher per worktree".to_string(),
            body: "The election lock guarantees a single watcher; never bind without it."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: None,
            tags: vec![],
            bind: RepoMemoryBindTarget {
                symbol_id: Some(1),
                logical_symbol_id: None,
                chunk_id: None,
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
            },
        },
    )
    .unwrap();
    schema::rebuild_fts(&conn).unwrap();
    conn
}

#[test]
fn compose_identifier_pattern_yields_symbol_and_memory() {
    let conn = seeded_conn();
    let out = compose(&conn, r"watcher_main\b", None, &DedupeFilter::default())
        .unwrap()
        .expect("payload expected");
    assert!(out.context.contains("src/watch.rs"), "symbol location present");
    assert!(out.context.contains("One watcher per worktree"), "memory title present");
    let memory_pos = out.context.find("One watcher per worktree").unwrap();
    let symbol_pos = out.context.find("src/watch.rs").unwrap();
    assert!(memory_pos < symbol_pos, "memories render before symbols");
    assert_eq!(out.memory_ids.len(), 1);
    assert_eq!(out.symbol_keys.len(), 1);
    assert!(out.context.len() <= MAX_CONTEXT_CHARS);
}

#[test]
fn compose_respects_dedupe_filter_and_returns_none_when_everything_filtered() {
    let conn = seeded_conn();
    let first = compose(&conn, "watcher_main", None, &DedupeFilter::default())
        .unwrap()
        .expect("first payload");
    let filter = DedupeFilter {
        memory_ids: first.memory_ids.iter().cloned().collect::<HashSet<_>>(),
        symbol_keys: first.symbol_keys.iter().cloned().collect::<HashSet<_>>(),
    };
    assert!(compose(&conn, "watcher_main", None, &filter).unwrap().is_none());
}

#[test]
fn compose_non_identifier_pattern_uses_lexical_lane() {
    let conn = seeded_conn();
    let out = compose(&conn, "election retry loop", None, &DedupeFilter::default())
        .unwrap()
        .expect("lexical payload");
    assert!(out.context.contains("src/watch.rs"));
}

#[test]
fn compose_unknown_pattern_yields_none() {
    let conn = seeded_conn();
    assert!(compose(&conn, "zzqqyyxx_nothing", None, &DedupeFilter::default()).unwrap().is_none());
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p rag-rat-core grep_augment::`
Expected: FAIL to compile — `compose`, `DedupeFilter`, `MAX_CONTEXT_CHARS` undefined. (If a seed INSERT trips a NOT NULL/CHECK, align the column list with `index/schema.rs` — canonical definitions at lines 230/245/269/314.)

- [ ] **Step 3: Implement**

```rust
use std::collections::HashSet;

use rusqlite::Connection;

use crate::query::{memory, symbol};
use crate::search::lexical;

/// Hard cap on rendered context. Truncation drops whole items, never mid-item.
pub const MAX_CONTEXT_CHARS: usize = 1500;
const MAX_SYMBOLS: u32 = 3;
const MAX_MEMORIES: u32 = 4;
const MAX_LEXICAL_HITS: u32 = 3;

/// What the listener/fallback already injected for this session. Default = inject everything.
#[derive(Debug, Default, Clone)]
pub struct DedupeFilter {
    pub memory_ids: HashSet<String>,
    pub symbol_keys: HashSet<String>,
}

/// A rendered digest plus the IDs it contains, for the caller's dedupe bookkeeping.
#[derive(Debug)]
pub struct GrepAugment {
    pub context: String,
    pub memory_ids: Vec<String>,
    pub symbol_keys: Vec<String>,
}

/// Compose the grep-augmentation digest for one search. Lanes per the spec: symbol lane when
/// the pattern looks like an identifier, memory lane always, lexical lane only when the
/// symbol lane is empty. Returns `None` when nothing (new) is worth injecting.
pub fn compose(
    conn: &Connection,
    raw_pattern: &str,
    search_path: Option<&str>,
    dedupe: &DedupeFilter,
) -> anyhow::Result<Option<GrepAugment>> {
    let normalized = normalize_pattern(raw_pattern);
    if normalized.is_empty() {
        return Ok(None);
    }

    let mut memories = Vec::new();
    let mut symbol_lines = Vec::new();
    let mut symbol_keys = Vec::new();

    if let Some(ident) = identifier_candidate(&normalized) {
        // Symbol lane. Bare name for qualified queries: `Watcher::spawn` → `spawn`.
        let bare = ident.rsplit([':', '.']).next().unwrap_or(ident);
        for hit in symbol::lookup(conn, bare, None, MAX_SYMBOLS)? {
            let key = format!("{}:{}", hit.path, hit.qualified_name);
            if dedupe.symbol_keys.contains(&key) {
                continue;
            }
            let (callers, callees) = edge_counts(conn, &hit)?;
            let start_line = line_for_symbol(conn, &hit)?;
            symbol_lines.push(format!(
                "- `{}` ({}) — {}:{} — {} callers / {} callees{}",
                hit.qualified_name,
                hit.kind,
                hit.path,
                start_line,
                callers,
                callees,
                hit.signature.as_deref().map(|s| format!(" — `{s}`")).unwrap_or_default(),
            ));
            memories.extend(memory::memories_for_symbol(conn, &hit, MAX_MEMORIES)?);
            symbol_keys.push(key);
        }
    }

    // Memory lane: always. FTS over the normalized pattern + path-bound memories.
    memories.extend(memory::memory_search(conn, &normalized, MAX_MEMORIES)?);
    if let Some(path) = search_path {
        memories.extend(memory::memories_for_path(conn, path, MAX_MEMORIES)?);
    }
    memories.sort_by(|a, b| a.memory_id.cmp(&b.memory_id));
    memories.dedup_by(|a, b| a.memory_id == b.memory_id);
    memories.retain(|m| !dedupe.memory_ids.contains(&m.memory_id));

    // Lexical lane: only when the symbol lane found nothing.
    let lexical_lines = if symbol_lines.is_empty() {
        lexical::search_lexical_only(conn, &normalized, MAX_LEXICAL_HITS, false)?
            .into_iter()
            .map(|hit| format!("- {}:{}-{} — {}", hit.path, hit.start_line, hit.end_line, hit.summary))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    if memories.is_empty() && symbol_lines.is_empty() && lexical_lines.is_empty() {
        return Ok(None);
    }
    Ok(Some(render(memories, symbol_lines, symbol_keys, lexical_lines)))
}

/// Caller/callee edge counts. Callers resolve by `to_symbol_id` or qualified-name match;
/// callees are edges leaving any of the symbol's concrete rows.
fn edge_counts(conn: &Connection, hit: &symbol::SymbolHit) -> anyhow::Result<(i64, i64)> {
    let callers: i64 = conn.query_row(
        "SELECT COUNT(*) FROM edges WHERE to_symbol_id = ?1 OR target_qualified_name = ?2",
        rusqlite::params![hit.symbol_id, hit.qualified_name],
        |row| row.get(0),
    )?;
    let callees: i64 = conn.query_row(
        "SELECT COUNT(*) FROM edges WHERE from_symbol_id = ?1",
        [hit.symbol_id],
        |row| row.get(0),
    )?;
    Ok((callers, callees))
}

/// 1-based start line for a symbol hit (line spans live on chunks; fall back to byte offset 0 → 1).
fn line_for_symbol(conn: &Connection, hit: &symbol::SymbolHit) -> anyhow::Result<i64> {
    let line: Option<i64> = conn
        .query_row(
            "SELECT start_line FROM chunks
             WHERE file_id = ?1 AND start_byte <= ?2 AND end_byte >= ?2
             ORDER BY (end_byte - start_byte) ASC LIMIT 1",
            rusqlite::params![hit.file_id, hit.start_byte],
            |row| row.get(0),
        )
        .optional()?;
    Ok(line.unwrap_or(1))
}

/// Memories first (the unique signal), then symbols, then lexical hits; whole-item truncation
/// against `MAX_CONTEXT_CHARS`.
fn render(
    memories: Vec<memory::RepoMemory>,
    symbol_lines: Vec<String>,
    symbol_keys: Vec<String>,
    lexical_lines: Vec<String>,
) -> GrepAugment {
    let mut sections = Vec::new();
    let mut memory_ids = Vec::new();
    if !memories.is_empty() {
        let mut lines = vec!["**Repo memories bound to this code:**".to_string()];
        for m in &memories {
            lines.push(format!(
                "- [{} | {}] {} — {} (rag-rat: memory_search)",
                m.kind, m.status, m.title, m.body
            ));
            memory_ids.push(m.memory_id.clone());
        }
        sections.push(lines);
    }
    if !symbol_lines.is_empty() {
        let mut lines = vec!["**Known symbols matching this pattern:**".to_string()];
        lines.extend(symbol_lines);
        lines.push("(rag-rat: impact_surface <name> before editing)".to_string());
        sections.push(lines);
    }
    if !lexical_lines.is_empty() {
        let mut lines = vec!["**Indexed hits (rag-rat semantic_search has more):**".to_string()];
        lines.extend(lexical_lines);
        sections.push(lines);
    }
    let mut context = String::from("rag-rat index context for this search:\n");
    'outer: for section in sections {
        for line in section {
            if context.len() + line.len() + 1 > MAX_CONTEXT_CHARS {
                break 'outer;
            }
            context.push_str(&line);
            context.push('\n');
        }
    }
    GrepAugment { context: context.trim_end().to_string(), memory_ids, symbol_keys }
}
```

Add `use rusqlite::OptionalExtension;` as needed. Note: `memories_for_symbol` already covers symbol-bound memories; the body-render trims long bodies implicitly via the char cap.

- [ ] **Step 4: Run tests** — `cargo test -p rag-rat-core grep_augment::` Expected: PASS. Also run `cargo test -p rag-rat-core` (full crate) to catch regressions.

- [ ] **Step 5: Commit**

```bash
git add crates/rag-rat-core/src/query/grep_augment.rs
git commit -m "feat(query): grep_augment payload composition with dedupe filter"
```

---

### Task 6: `rag-rat-mcp::claude_hook` — wire protocol types

**Files:**
- Create: `crates/rag-rat-mcp/src/claude_hook.rs`
- Modify: `crates/rag-rat-mcp/src/lib.rs` (add `pub mod claude_hook;`)

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrips_and_tolerates_unknown_fields() {
        let json = r#"{"v":1,"kind":"grep_augment","session_id":"s1","pattern":"foo",
                       "search_path":null,"source":"grep_tool","future_field":true}"#;
        let req: HookRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.v, 1);
        assert_eq!(req.kind, "grep_augment");
        assert_eq!(req.pattern, "foo");
        assert!(req.search_path.is_none());
    }

    #[test]
    fn response_serializes_null_context_explicitly() {
        let resp = HookResponse { v: 1, context: None };
        assert_eq!(serde_json::to_string(&resp).unwrap(), r#"{"v":1,"context":null}"#);
    }
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p rag-rat-mcp claude_hook::` Expected: compile FAIL.

- [ ] **Step 3: Implement**

```rust
//! Unix-socket listener serving the Claude Code grep-augmentation PreToolUse hook.
//!
//! One listener per worktree (socket election lock); newline-delimited JSON, one request per
//! connection; per-session dedupe in memory. Read-only on the index by construction. Spec:
//! `docs/specs/2026-06-09-grep-augment-pretooluse-hook.md`.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

/// One grep-augment query from a hook client. Unknown fields are ignored (forward compat);
/// unknown `v`/`kind` get a null-context reply rather than an error.
#[derive(Debug, Deserialize)]
pub struct HookRequest {
    pub v: u32,
    pub kind: String,
    pub session_id: String,
    pub pattern: String,
    #[serde(default)]
    pub search_path: Option<String>,
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Serialize)]
pub struct HookResponse {
    pub v: u32,
    pub context: Option<String>,
}
```

- [ ] **Step 4: Run tests** — `cargo test -p rag-rat-mcp claude_hook::` Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/rag-rat-mcp/src/claude_hook.rs crates/rag-rat-mcp/src/lib.rs
git commit -m "feat(mcp): claude_hook wire protocol types"
```

---

### Task 7: `rag-rat-mcp::claude_hook` — elected socket listener with session dedupe

**Files:**
- Modify: `crates/rag-rat-mcp/src/claude_hook.rs`

- [ ] **Step 1: Write the failing test** (tokio integration-style test inside the module; Unix-gated)

```rust
#[cfg(all(test, unix))]
mod listener_tests {
    use std::time::Duration;

    use rag_rat_core::{config::Config, index::schema, storage::IndexConnection};

    use super::*;

    fn test_config() -> Config {
        let root = std::env::temp_dir().join(format!(
            "ragrat-hooksock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(root.join(".rag-rat")).unwrap();
        let database = root.join(".rag-rat/index.db");
        let rw = IndexConnection::open(&database).unwrap();
        schema::apply(rw.connection()).unwrap();
        rw.connection()
            .execute(
                "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
                 VALUES ('src/lib.rs', 'rust', 'source', 'abc', 0, 0)",
                [],
            )
            .unwrap();
        rw.connection()
            .execute(
                "INSERT INTO symbols(file_id, language, name, qualified_name, kind, start_byte,
                                     end_byte, signature, docs)
                 VALUES (1, 'rust', 'frobnicate', 'lib::frobnicate', 'function', 0, 10,
                         'fn frobnicate()', NULL)",
                [],
            )
            .unwrap();
        schema::rebuild_fts(rw.connection()).unwrap();
        // Build a minimal Config pointing at this root/database. Use Config::load on a written
        // rag-rat.toml if Config fields are non-constructible; the existing
        // crates/rag-rat-cli/tests/mcp_hot_upgrade.rs TestEnv shows the minimal toml shape.
        config_for(root, database)
    }

    async fn request(socket: &std::path::Path, body: serde_json::Value) -> serde_json::Value {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
        let (read, mut write) = stream.into_split();
        write.write_all(format!("{body}\n").as_bytes()).await.unwrap();
        let mut line = String::new();
        BufReader::new(read).read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[tokio::test]
    async fn listener_serves_context_then_dedupes_per_session() {
        let config = test_config();
        let _listener = spawn_listener(config.clone());
        let socket = socket_path_for(&config);
        // Election + bind are async; poll for the socket.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !socket.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let req = |sid: &str| {
            serde_json::json!({"v": 1, "kind": "grep_augment", "session_id": sid,
                               "pattern": "frobnicate", "search_path": null, "source": "grep_tool"})
        };
        let first = request(&socket, req("s1")).await;
        assert!(first["context"].as_str().unwrap().contains("lib::frobnicate"));
        let second = request(&socket, req("s1")).await;
        assert!(second["context"].is_null(), "same session deduped");
        let other = request(&socket, req("s2")).await;
        assert!(other["context"].as_str().unwrap().contains("lib::frobnicate"),
            "fresh session not deduped");
        let bad = request(&socket, serde_json::json!({"v": 99, "kind": "nope"})).await;
        assert!(bad["context"].is_null(), "unknown version answered, not errored");
    }

    #[tokio::test]
    async fn second_listener_takes_over_when_winner_dies() {
        let config = test_config();
        let winner = spawn_listener(config.clone());
        let socket = socket_path_for(&config);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !socket.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // The loser parks in the election retry loop while the winner holds the lock.
        let loser = spawn_listener(config.clone());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!loser.is_finished(), "loser must wait, not exit");

        // Kill the winner: its lock fd and bound socket drop with the task's process state.
        winner.abort();
        let _ = winner.await;
        // NOTE: in-process abort drops the FileLock (held by the task) but the dead socket
        // file remains — exactly the stale-socket case. The loser must unlink + re-bind.
        let req = serde_json::json!({"v": 1, "kind": "grep_augment", "session_id": "takeover",
                                     "pattern": "frobnicate", "search_path": null,
                                     "source": "grep_tool"});
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            // Election retry is 5s; poll until the loser owns the socket and answers.
            if let Ok(stream) = tokio::net::UnixStream::connect(&socket).await {
                drop(stream);
                let reply = request(&socket, req.clone()).await;
                assert!(reply["context"].as_str().unwrap().contains("lib::frobnicate"));
                break;
            }
            assert!(std::time::Instant::now() < deadline, "loser never took over the socket");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        loser.abort();
    }
}
```

Implementation note this test forces: the election loop must hold the `FileLock` inside the
spawned task (so aborting the task drops it) — the Task 7 implementation already does this
(`let _lock: FileLock = loop { … }` inside the async block). If the takeover test hangs, check
that the lock isn't leaked to a detached scope.

(`config_for` is the test-local constructor: write a minimal `rag-rat.toml` into `root` and `Config::load` it — copy the toml shape from `crates/rag-rat-cli/tests/mcp_hot_upgrade.rs`'s `TestEnv::setup`, overriding the database path.)

- [ ] **Step 2: Run to verify failure** — `cargo test -p rag-rat-mcp listener_serves` Expected: compile FAIL (`spawn_listener`, `socket_path_for` undefined).

- [ ] **Step 3: Implement** (append to `claude_hook.rs`, all `#[cfg(unix)]`)

```rust
#[cfg(unix)]
pub use listener::{spawn_listener, socket_path_for};

#[cfg(unix)]
mod listener {
    use std::{
        collections::HashMap,
        path::PathBuf,
        time::{Duration, Instant},
    };

    use rag_rat_core::{
        config::Config,
        locks::{self, FileLock},
        query::grep_augment::{self, DedupeFilter},
        storage::IndexConnection,
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::{UnixListener, UnixStream},
        task::JoinHandle,
    };

    use super::{HookRequest, HookResponse, PROTOCOL_VERSION};

    const ELECTION_RETRY: Duration = Duration::from_secs(5);
    const SESSION_CAP: usize = 64;
    const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

    pub fn socket_path_for(config: &Config) -> PathBuf {
        let base_dir = config
            .database
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| config.root.clone());
        locks::hook_socket_path(&base_dir, &config.root)
    }

    fn socket_lock_path_for(config: &Config) -> PathBuf {
        let base_dir = config
            .database
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| config.root.clone());
        locks::socket_lock_path(&base_dir, &config.root)
    }

    /// Per-session record of what was already injected. Pruned by LRU cap + TTL.
    #[derive(Default)]
    struct SessionState {
        filter: DedupeFilter,
        last_used: Option<Instant>,
    }

    /// Spawn the hook listener task: win the socket election (retrying forever, like the
    /// watcher), then accept hook clients until the task is dropped. Returns the JoinHandle so
    /// the server can abort it on teardown; the lock and socket release with the process.
    pub fn spawn_listener(config: Config) -> JoinHandle<()> {
        tokio::spawn(async move {
            let lock_path = socket_lock_path_for(&config);
            let _lock: FileLock = loop {
                match FileLock::try_acquire(&lock_path) {
                    Ok(Some(lock)) => break lock,
                    _ => tokio::time::sleep(ELECTION_RETRY).await,
                }
            };
            let socket = socket_path_for(&config);
            if let Some(parent) = socket.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Only the lock holder ever unlinks: race-free stale-socket cleanup.
            let _ = std::fs::remove_file(&socket);
            let Ok(listener) = UnixListener::bind(&socket) else { return };
            let mut sessions: HashMap<String, SessionState> = HashMap::new();
            loop {
                let Ok((stream, _addr)) = listener.accept().await else { continue };
                prune_sessions(&mut sessions);
                if let Err(err) = serve_one(stream, &config, &mut sessions).await {
                    if std::env::var_os("RAG_RAT_HOOK_DEBUG").is_some() {
                        eprintln!("claude-hook listener: {err:#}");
                    }
                }
            }
        })
    }

    fn prune_sessions(sessions: &mut HashMap<String, SessionState>) {
        let now = Instant::now();
        sessions.retain(|_, s| s.last_used.is_some_and(|t| now.duration_since(t) < SESSION_TTL));
        while sessions.len() > SESSION_CAP {
            let oldest = sessions
                .iter()
                .min_by_key(|(_, s)| s.last_used)
                .map(|(k, _)| k.clone());
            let Some(key) = oldest else { break };
            sessions.remove(&key);
        }
    }

    async fn serve_one(
        stream: UnixStream,
        config: &Config,
        sessions: &mut HashMap<String, SessionState>,
    ) -> anyhow::Result<()> {
        let (read, mut write) = stream.into_split();
        let mut line = String::new();
        BufReader::new(read).read_line(&mut line).await?;
        let reply = match serde_json::from_str::<HookRequest>(&line) {
            Ok(req) if req.v == PROTOCOL_VERSION && req.kind == "grep_augment" => {
                let state = sessions.entry(req.session_id.clone()).or_default();
                state.last_used = Some(Instant::now());
                let filter = state.filter.clone();
                let database = config.database.clone();
                let pattern = req.pattern.clone();
                let search_path = req.search_path.clone();
                // rusqlite is sync; one short read off the runtime threads.
                let composed = tokio::task::spawn_blocking(move || {
                    let conn = IndexConnection::open_read_only(&database)?;
                    grep_augment::compose(
                        conn.connection(),
                        &pattern,
                        search_path.as_deref(),
                        &filter,
                    )
                })
                .await??;
                match composed {
                    Some(out) => {
                        let state = sessions.entry(req.session_id).or_default();
                        state.filter.memory_ids.extend(out.memory_ids.iter().cloned());
                        state.filter.symbol_keys.extend(out.symbol_keys.iter().cloned());
                        HookResponse { v: PROTOCOL_VERSION, context: Some(out.context) }
                    },
                    None => HookResponse { v: PROTOCOL_VERSION, context: None },
                }
            },
            _ => HookResponse { v: PROTOCOL_VERSION, context: None },
        };
        let mut payload = serde_json::to_string(&reply)?;
        payload.push('\n');
        write.write_all(payload.as_bytes()).await?;
        Ok(())
    }
}
```

(If `DedupeFilter` lacks `Clone`, add `#[derive(Clone)]` in Task 5's struct — already specified there.)

- [ ] **Step 4: Run tests** — `cargo test -p rag-rat-mcp claude_hook` Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/rag-rat-mcp/src/claude_hook.rs
git commit -m "feat(mcp): elected Unix-socket listener for grep-augment hook"
```

---

### Task 8: Wire the listener into `run_stdio`

**Files:**
- Modify: `crates/rag-rat-mcp/src/server.rs:452-512`

- [ ] **Step 1: Implement** (no new test yet — Task 12's e2e covers it; this is two lines per branch)

In `run_stdio_unix` (server.rs:471), right after the `Watcher::spawn_with_fleet` line:

```rust
    // Grep-augment hook listener: one per worktree via the socket election; aborts on drop.
    let hook_listener = crate::claude_hook::spawn_listener(config.clone());
```

And ensure it is aborted on teardown — at the end of `run_stdio_unix` where the function returns/exits (after `running.waiting()`-equivalent), add:

```rust
    hook_listener.abort();
```

(Match the existing teardown structure: if the function has multiple exit paths, a small guard struct `struct AbortOnDrop(JoinHandle<()>); impl Drop { fn drop(&mut self) { self.0.abort(); } }` bound as `let _hook_listener = AbortOnDrop(crate::claude_hook::spawn_listener(config.clone()));` is the cleaner shape — prefer it.)

The non-Unix branch of `run_stdio` gets no listener (spec: fallback-only on non-Unix).

- [ ] **Step 2: Verify it builds and existing tests pass**

Run: `cargo build && cargo test -p rag-rat-cli --test mcp_stdio && cargo test -p rag-rat-cli --test mcp_hot_upgrade`
Expected: PASS — existing stdio + hot-upgrade behavior unchanged.

- [ ] **Step 3: Commit**

```bash
git add crates/rag-rat-mcp/src/server.rs
git commit -m "feat(mcp): start grep-augment hook listener with the stdio server"
```

---

### Task 9: CLI — PreToolUse input parsing + Bash command parser

**Files:**
- Create: `crates/rag-rat-cli/src/claude_hook.rs`
- Modify: `crates/rag-rat-cli/src/main.rs` (add `mod claude_hook;` next to `mod init;`)

- [ ] **Step 1: Write the failing tests** (in `claude_hook.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_grep_tool_input() {
        let json = r#"{"session_id":"s1","cwd":"/repo","hook_event_name":"PreToolUse",
            "tool_name":"Grep","tool_input":{"pattern":"watcher_main","path":"crates"}}"#;
        let input: HookInput = serde_json::from_str(json).unwrap();
        let search = extract_search(&input).unwrap();
        assert_eq!(search.pattern, "watcher_main");
        assert_eq!(search.search_path.as_deref(), Some("crates"));
        assert_eq!(search.source, "grep_tool");
    }

    #[test]
    fn bash_parser_table() {
        // (command, expected pattern, expected path)
        let positives = [
            ("rg watcher_main", "watcher_main", None),
            ("rg -n 'election retry' crates/", "election retry", Some("crates/")),
            ("grep -rn foo src", "foo", Some("src")),
            ("ag --rust frobnicate", "frobnicate", None),
            ("rg -e 'fn main' --type rust", "fn main", None),
            ("cd crates && rg spawn_listener", "spawn_listener", None),
            ("FOO=1 rg spawn_listener", "spawn_listener", None),
            ("rg -A 3 -B 2 needle haystack/", "needle", Some("haystack/")),
            ("git log | rg fix", "fix", None),
            (r#"rg "quoted pattern" src"#, "quoted pattern", Some("src")),
        ];
        for (cmd, pattern, path) in positives {
            let got = parse_bash_search(cmd).unwrap_or_else(|| panic!("no match for {cmd}"));
            assert_eq!(got.0, pattern, "pattern for {cmd}");
            assert_eq!(got.1.as_deref(), path, "path for {cmd}");
        }
        let negatives = [
            "ls -la",
            "cargo test",
            "rg",                       // no pattern
            "find . -name '*.rs' -exec grep foo {} \\;", // -exec: ambiguous
            "echo `rg foo`",            // backticks: ambiguous
            "xargs grep foo",           // xargs: ambiguous
            "groups",                   // not grep
        ];
        for cmd in negatives {
            assert!(parse_bash_search(cmd).is_none(), "false positive for {cmd}");
        }
    }

    #[test]
    fn extract_search_routes_bash_commands() {
        let json = r#"{"session_id":"s1","cwd":"/repo","hook_event_name":"PreToolUse",
            "tool_name":"Bash","tool_input":{"command":"rg -n watcher_main crates/"}}"#;
        let input: HookInput = serde_json::from_str(json).unwrap();
        let search = extract_search(&input).unwrap();
        assert_eq!(search.pattern, "watcher_main");
        assert_eq!(search.source, "bash");
    }

    #[test]
    fn extract_search_ignores_other_tools() {
        let json = r#"{"session_id":"s1","cwd":"/repo","hook_event_name":"PreToolUse",
            "tool_name":"Read","tool_input":{"path":"/x"}}"#;
        let input: HookInput = serde_json::from_str(json).unwrap();
        assert!(extract_search(&input).is_none());
    }
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p rag-rat-cli claude_hook::` Expected: compile FAIL.

- [ ] **Step 3: Implement**

```rust
//! `rag-rat claude-hook`: the Claude Code PreToolUse hook client.
//!
//! Reads the hook JSON from stdin, asks the elected listener (or falls back to a direct
//! read-only query), and prints `additionalContext` JSON. Exit 0 on every path — the hook
//! augments greps and must never block one. Spec:
//! `docs/specs/2026-06-09-grep-augment-pretooluse-hook.md`.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct HookInput {
    pub session_id: String,
    pub cwd: String,
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: serde_json::Value,
}

pub struct Search {
    pub pattern: String,
    pub search_path: Option<String>,
    pub source: &'static str,
}

/// Pull a search intent out of the hook input; `None` means "not a grep, stay silent".
pub fn extract_search(input: &HookInput) -> Option<Search> {
    match input.tool_name.as_str() {
        "Grep" => {
            let pattern = input.tool_input.get("pattern")?.as_str()?.to_string();
            let search_path =
                input.tool_input.get("path").and_then(|v| v.as_str()).map(str::to_string);
            Some(Search { pattern, search_path, source: "grep_tool" })
        },
        "Bash" => {
            let command = input.tool_input.get("command")?.as_str()?;
            let (pattern, search_path) = parse_bash_search(command)?;
            Some(Search { pattern, search_path, source: "bash" })
        },
        _ => None,
    }
}

const SEARCH_COMMANDS: &[&str] = &["grep", "rg", "ag"];
/// Flags whose *next* token is a value, not the pattern. Conservative superset across the
/// three tools — a missed flag only costs a wrong-pattern no-op downstream, never a block.
const ARG_FLAGS: &[&str] = &[
    "-A", "-B", "-C", "-m", "-g", "-t", "-T", "-f", "-M", "--glob", "--type", "--type-not",
    "--include", "--exclude", "--exclude-dir", "--max-count", "--max-depth", "--context",
    "--after-context", "--before-context", "--file", "--ignore-file", "--threads", "--colors",
];

/// Extract (pattern, path) from a shell command that runs grep/rg/ag, or `None` when the
/// command doesn't or parsing would have to guess. False negatives are fine; false
/// positives are not (spec: Bash command parsing).
pub fn parse_bash_search(command: &str) -> Option<(String, Option<String>)> {
    if command.contains('`') || command.contains("$(") {
        return None; // substitution: ambiguous
    }
    // Split into pipeline/sequence segments; examine each for a search command.
    for segment in split_top_level(command) {
        let tokens = shell_tokens(&segment)?;
        let mut tokens = tokens.as_slice();
        // Skip env-var prefixes (FOO=bar) before the command word.
        while tokens.first().is_some_and(|t| t.contains('=') && !t.starts_with('-')) {
            tokens = &tokens[1..];
        }
        let Some(command_word) = tokens.first() else { continue };
        let base = command_word.rsplit('/').next().unwrap_or(command_word);
        if base == "xargs" || base == "find" {
            return None; // grep as an argument of these is ambiguous
        }
        if !SEARCH_COMMANDS.contains(&base) {
            continue;
        }
        let mut pattern: Option<String> = None;
        let mut path: Option<String> = None;
        let mut rest = tokens[1..].iter();
        while let Some(token) = rest.next() {
            if let Some(value) = token.strip_prefix("--regexp=") {
                pattern.get_or_insert_with(|| value.to_string());
            } else if token == "-e" || token == "--regexp" {
                if let Some(value) = rest.next() {
                    pattern.get_or_insert_with(|| value.to_string());
                }
            } else if ARG_FLAGS.contains(&token.as_str()) {
                rest.next(); // consume the flag's value
            } else if token.starts_with('-') && token.len() > 1 {
                // value-less flag (or unknown): skip
            } else if pattern.is_none() {
                pattern = Some(token.to_string());
            } else if path.is_none() {
                path = Some(token.to_string());
            }
        }
        return pattern.map(|p| (p, path));
    }
    None
}

/// Split on top-level `|`, `&&`, `||`, `;` (quote-aware); also drop a leading `cd …` segment.
fn split_top_level(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '\'' | '"') => quote = Some(ch),
            (None, '|' | ';') => {
                if chars.peek() == Some(&'|') {
                    chars.next();
                }
                segments.push(std::mem::take(&mut current));
                continue;
            },
            (None, '&') => {
                if chars.peek() == Some(&'&') {
                    chars.next();
                }
                segments.push(std::mem::take(&mut current));
                continue;
            },
            (None, c) => current.push(c),
        }
        if quote.is_some() {
            // keep quoted chars verbatim (already pushed above)
        }
    }
    segments.push(current);
    segments
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !s.starts_with("cd ") && *s != "cd")
        .collect()
}

/// Quote-aware tokenization of one segment. `None` on unbalanced quotes (ambiguous).
fn shell_tokens(segment: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in segment.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '\'' | '"') => quote = Some(ch),
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            },
            (None, c) => current.push(c),
        }
    }
    if quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Some(tokens)
}
```

Note on quoting in `split_top_level`: the quoted characters must be preserved *including* for later tokenization — the segment keeps its quote characters. Simplest correct form: when entering/leaving quotes in `split_top_level`, still push the quote char to `current` (so `shell_tokens` sees and strips them). Adjust both match arms accordingly; the `bash_parser_table` test (quoted pattern row) is the guard.

- [ ] **Step 4: Run tests** — `cargo test -p rag-rat-cli claude_hook::` Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/rag-rat-cli/src/claude_hook.rs crates/rag-rat-cli/src/main.rs
git commit -m "feat(cli): PreToolUse input parsing + conservative bash search parser"
```

---

### Task 10: CLI — hook client flow (socket → fallback → print), main dispatch

**Files:**
- Modify: `crates/rag-rat-cli/src/claude_hook.rs`
- Modify: `crates/rag-rat-cli/src/main.rs:22-30` (dispatch **before** `Config::load` — like `init`)
- Test: `crates/rag-rat-cli/tests/claude_hook_e2e.rs` (created here with the no-op tests; e2e socket tests land in Task 12)

- [ ] **Step 1: Write the failing tests** (new integration test file)

```rust
//! End-to-end tests for `rag-rat claude-hook` (Unix only for socket paths; the no-op
//! contract tests run everywhere).

use std::{
    io::Write,
    process::{Command, Stdio},
};

fn run_hook(stdin_body: &str, cwd: &std::path::Path) -> (String, std::process::ExitStatus) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .arg("claude-hook")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(stdin_body.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (String::from_utf8_lossy(&out.stdout).into_owned(), out.status)
}

#[test]
fn no_rag_rat_toml_means_silent_exit_zero() {
    let dir = std::env::temp_dir().join(format!("ragrat-hook-noindex-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = serde_json::json!({
        "session_id": "s1", "cwd": dir, "hook_event_name": "PreToolUse",
        "tool_name": "Grep", "tool_input": {"pattern": "anything"}
    });
    let (stdout, status) = run_hook(&input.to_string(), &dir);
    assert!(status.success());
    assert!(stdout.is_empty(), "must print nothing without an index, got: {stdout}");
}

#[test]
fn garbage_stdin_means_silent_exit_zero() {
    let dir = std::env::temp_dir().join(format!("ragrat-hook-garbage-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (stdout, status) = run_hook("this is not json", &dir);
    assert!(status.success());
    assert!(stdout.is_empty());
}

#[test]
fn non_search_tool_means_silent_exit_zero() {
    let dir = std::env::temp_dir().join(format!("ragrat-hook-read-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = serde_json::json!({
        "session_id": "s1", "cwd": dir, "hook_event_name": "PreToolUse",
        "tool_name": "Read", "tool_input": {"path": "/x"}
    });
    let (stdout, status) = run_hook(&input.to_string(), &dir);
    assert!(status.success());
    assert!(stdout.is_empty());
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p rag-rat-cli --test claude_hook_e2e`
Expected: FAIL — `claude-hook` is an unknown subcommand (non-zero exit / usage output).

- [ ] **Step 3: Implement the client flow** (append to `claude_hook.rs`)

```rust
use std::{
    io::Read as _,
    path::{Path, PathBuf},
};

use rag_rat_core::{config::Config, query::grep_augment, storage::IndexConnection};

const SOCKET_BUDGET: std::time::Duration = std::time::Duration::from_millis(250);

/// Entry point for `rag-rat claude-hook`. Every failure path prints nothing and returns
/// Ok(()) — the hook must never block a grep (spec: error posture).
pub fn run() -> anyhow::Result<()> {
    let _ = run_inner(); // swallow: silence is the contract
    Ok(())
}

fn run_inner() -> anyhow::Result<()> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let input: HookInput = serde_json::from_str(&raw)?;
    let Some(search) = extract_search(&input) else { return Ok(()) };
    let Some(config) = find_config(Path::new(&input.cwd)) else { return Ok(()) };

    let context = ask_listener(&config, &input.session_id, &search)
        .unwrap_or_else(|| fallback_compose(&config, &search));
    if let Some(context) = context {
        // PreToolUse contract: allow + additionalContext; plain stdout is debug-only.
        println!(
            "{}",
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "allow",
                    "additionalContext": context,
                }
            })
        );
    }
    Ok(())
}

/// Walk up from the hook's cwd to the nearest rag-rat.toml. `None` ⇒ not a rag-rat repo ⇒
/// silent no-op (what makes `--global` install safe).
fn find_config(start: &Path) -> Option<Config> {
    let mut dir = Some(start);
    while let Some(current) = dir {
        let candidate = current.join("rag-rat.toml");
        if candidate.is_file() {
            return Config::load(&candidate).ok();
        }
        dir = current.parent();
    }
    None
}

/// Outer Option: did the listener answer at all (None ⇒ fall back). Inner Option: did it
/// have anything new to say.
fn ask_listener(config: &Config, session_id: &str, search: &Search) -> Option<Option<String>> {
    #[cfg(unix)]
    {
        use std::{io::{BufRead, BufReader, Write as _}, os::unix::net::UnixStream};
        let socket = socket_path(config);
        let stream = UnixStream::connect(&socket).ok()?;
        stream.set_read_timeout(Some(SOCKET_BUDGET)).ok()?;
        stream.set_write_timeout(Some(SOCKET_BUDGET)).ok()?;
        let request = serde_json::json!({
            "v": 1, "kind": "grep_augment", "session_id": session_id,
            "pattern": search.pattern, "search_path": search.search_path,
            "source": search.source,
        });
        let mut writer = stream.try_clone().ok()?;
        writeln!(writer, "{request}").ok()?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).ok()?;
        let reply: serde_json::Value = serde_json::from_str(&line).ok()?;
        if reply.get("v")?.as_u64()? != 1 {
            return None;
        }
        Some(reply.get("context")?.as_str().map(str::to_string))
    }
    #[cfg(not(unix))]
    {
        let _ = (config, session_id, search);
        None
    }
}

/// Mirrors `rag_rat_mcp::claude_hook::socket_path_for` without depending on the MCP crate:
/// both derive the path from the same `locks::hook_socket_path` inputs.
fn socket_path(config: &Config) -> PathBuf {
    let base_dir = config
        .database
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config.root.clone());
    rag_rat_core::locks::hook_socket_path(&base_dir, &config.root)
}

/// Stateless direct read (no dedupe — spec: fallback path). Any error ⇒ silence.
fn fallback_compose(config: &Config, search: &Search) -> Option<String> {
    let conn = IndexConnection::open_read_only(&config.database).ok()?;
    grep_augment::compose(
        conn.connection(),
        &search.pattern,
        search.search_path.as_deref(),
        &grep_augment::DedupeFilter::default(),
    )
    .ok()
    .flatten()
    .map(|out| out.context)
}
```

In `main.rs`, before the `Config::load` line (`main.rs:27`), mirroring the `init` early-exit:

```rust
    if command == "claude-hook" {
        return claude_hook::run();
    }
```

Also add `claude-hook` to the `usage()` string (`main.rs:892`).

- [ ] **Step 4: Run tests**

Run: `cargo test -p rag-rat-cli --test claude_hook_e2e && cargo test -p rag-rat-cli claude_hook::`
Expected: PASS — all three no-op contracts + the parser unit tests.

- [ ] **Step 5: Commit**

```bash
git add crates/rag-rat-cli/src/claude_hook.rs crates/rag-rat-cli/src/main.rs crates/rag-rat-cli/tests/claude_hook_e2e.rs
git commit -m "feat(cli): claude-hook client with socket-then-fallback flow"
```

---

### Task 11: CLI — `hooks install/uninstall/status --claude [--global]`

**Files:**
- Create: `crates/rag-rat-cli/src/claude_settings.rs`
- Modify: `crates/rag-rat-cli/src/main.rs` (in `fn hooks`, route to the new module when `--claude` is present; add `mod claude_settings;`)

- [ ] **Step 1: Write the failing tests** (in `claude_settings.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_into_empty_settings_creates_both_matchers() {
        let mut settings = serde_json::json!({});
        let changed = merge_hook_entries(&mut settings);
        assert!(changed);
        let entries = settings["hooks"]["PreToolUse"].as_array().unwrap();
        let matchers: Vec<&str> =
            entries.iter().map(|e| e["matcher"].as_str().unwrap()).collect();
        assert!(matchers.contains(&"Grep") && matchers.contains(&"Bash"));
        for entry in entries {
            let hook = &entry["hooks"][0];
            assert_eq!(hook["command"], HOOK_COMMAND);
            assert_eq!(hook["timeout"], 10);
        }
    }

    #[test]
    fn install_is_idempotent_and_preserves_foreign_entries() {
        let mut settings = serde_json::json!({
            "permissions": {"allow": ["Bash(ls:*)"]},
            "hooks": {"PreToolUse": [
                {"matcher": "Edit", "hooks": [{"type": "command", "command": "other-tool"}]}
            ]}
        });
        assert!(merge_hook_entries(&mut settings));
        assert!(!merge_hook_entries(&mut settings), "second install is a no-op");
        let entries = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(entries.len(), 3, "foreign Edit entry preserved alongside Grep+Bash");
        assert_eq!(settings["permissions"]["allow"][0], "Bash(ls:*)");
    }

    #[test]
    fn uninstall_removes_only_ours_and_prunes_empty_containers() {
        let mut settings = serde_json::json!({});
        merge_hook_entries(&mut settings);
        assert!(remove_hook_entries(&mut settings));
        assert!(settings.get("hooks").is_none(), "empty containers pruned");

        let mut mixed = serde_json::json!({
            "hooks": {"PreToolUse": [
                {"matcher": "Edit", "hooks": [{"type": "command", "command": "other-tool"}]}
            ]}
        });
        merge_hook_entries(&mut mixed);
        remove_hook_entries(&mut mixed);
        let entries = mixed["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["matcher"], "Edit");
    }

    #[test]
    fn status_reports_per_matcher_presence() {
        let mut settings = serde_json::json!({});
        assert_eq!(hook_status(&settings), (false, false));
        merge_hook_entries(&mut settings);
        assert_eq!(hook_status(&settings), (true, true));
    }
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p rag-rat-cli claude_settings::` Expected: compile FAIL.

- [ ] **Step 3: Implement**

```rust
//! Claude Code settings.json management for the grep-augment PreToolUse hook
//! (`rag-rat hooks install|uninstall|status --claude [--global]`).
//!
//! Edits are additive and marker-aware: our entries are recognized by `HOOK_COMMAND`;
//! everything else in the file is preserved byte-for-byte at the JSON level (read → modify
//! → pretty-print 2-space, matching how Claude Code writes the file).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

pub const HOOK_COMMAND: &str = "rag-rat claude-hook";
const MATCHERS: &[&str] = &["Grep", "Bash"];

fn our_entry(matcher: &str) -> Value {
    json!({
        "matcher": matcher,
        "hooks": [{"type": "command", "command": HOOK_COMMAND, "timeout": 10}]
    })
}

fn is_ours(entry: &Value) -> bool {
    entry["hooks"]
        .as_array()
        .is_some_and(|hooks| hooks.iter().any(|h| h["command"] == HOOK_COMMAND))
}

/// Add missing Grep/Bash entries. Returns true when the document changed.
pub fn merge_hook_entries(settings: &mut Value) -> bool {
    if !settings.is_object() {
        *settings = json!({});
    }
    let entries = settings
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .map(|hooks| hooks.entry("PreToolUse").or_insert_with(|| json!([])));
    let Some(Value::Array(entries)) = entries else { return false };
    let mut changed = false;
    for matcher in MATCHERS {
        let present = entries.iter().any(|e| e["matcher"] == *matcher && is_ours(e));
        if !present {
            entries.push(our_entry(matcher));
            changed = true;
        }
    }
    changed
}

/// Remove our entries; prune `PreToolUse`/`hooks` containers that end up empty.
pub fn remove_hook_entries(settings: &mut Value) -> bool {
    let Some(entries) = settings
        .get_mut("hooks")
        .and_then(|h| h.get_mut("PreToolUse"))
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let before = entries.len();
    entries.retain(|e| !is_ours(e));
    let changed = entries.len() != before;
    if entries.is_empty() {
        settings["hooks"].as_object_mut().unwrap().remove("PreToolUse");
    }
    if settings["hooks"].as_object().is_some_and(serde_json::Map::is_empty) {
        settings.as_object_mut().unwrap().remove("hooks");
    }
    changed
}

/// (grep_installed, bash_installed) for `hooks status --claude`.
pub fn hook_status(settings: &Value) -> (bool, bool) {
    let installed = |matcher: &str| {
        settings["hooks"]["PreToolUse"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|e| e["matcher"] == matcher && is_ours(e)))
    };
    (installed("Grep"), installed("Bash"))
}

/// Project `.claude/settings.json` or, with `--global`, `~/.claude/settings.json`.
pub fn settings_path(repo_root: &Path, global: bool) -> anyhow::Result<PathBuf> {
    if global {
        let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME not set"))?;
        Ok(PathBuf::from(home).join(".claude/settings.json"))
    } else {
        Ok(repo_root.join(".claude/settings.json"))
    }
}

pub fn read_settings(path: &Path) -> anyhow::Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

pub fn write_settings(path: &Path, settings: &Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(settings)?))?;
    Ok(())
}
```

In `main.rs::hooks` (main.rs:559), branch at the top:

```rust
fn hooks(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let Some(subcommand) = args.get(1).map(String::as_str) else {
        anyhow::bail!("hooks command needs install, uninstall, or status");
    };
    if args.iter().any(|a| a == "--claude") {
        return claude_hooks(config, subcommand, args.iter().any(|a| a == "--global"));
    }
    // ... existing git-hook body unchanged ...
}

fn claude_hooks(config: &Config, subcommand: &str, global: bool) -> anyhow::Result<()> {
    let path = claude_settings::settings_path(&config.root, global)?;
    let mut settings = claude_settings::read_settings(&path)?;
    match subcommand {
        "install" => {
            let changed = claude_settings::merge_hook_entries(&mut settings);
            if changed {
                claude_settings::write_settings(&path, &settings)?;
            }
            print_json(&serde_json::json!({
                "status": if changed { "installed" } else { "already_installed" },
                "settings_path": path,
                "matchers": ["Grep", "Bash"],
            }))
        },
        "uninstall" => {
            let changed = claude_settings::remove_hook_entries(&mut settings);
            if changed {
                claude_settings::write_settings(&path, &settings)?;
            }
            print_json(&serde_json::json!({
                "status": if changed { "uninstalled" } else { "not_installed" },
                "settings_path": path,
            }))
        },
        "status" => {
            let (grep, bash) = claude_settings::hook_status(&settings);
            print_json(&serde_json::json!({
                "settings_path": path,
                "grep_matcher_installed": grep,
                "bash_matcher_installed": bash,
            }))
        },
        other => anyhow::bail!("unknown hooks subcommand `{other}`"),
    }
}
```

- [ ] **Step 4: Run tests** — `cargo test -p rag-rat-cli claude_settings::` Expected: PASS. Manual check: `cargo run -- hooks status --claude` in the worktree prints JSON with both matchers false.

- [ ] **Step 5: Commit**

```bash
git add crates/rag-rat-cli/src/claude_settings.rs crates/rag-rat-cli/src/main.rs
git commit -m "feat(cli): hooks install/uninstall/status --claude [--global]"
```

---

### Task 12: End-to-end integration — listener + client + dedupe + fallback

**Files:**
- Modify: `crates/rag-rat-cli/tests/claude_hook_e2e.rs`

- [ ] **Step 1: Write the failing test** (Unix-gated; reuse the `TestEnv` toml/index shape from `crates/rag-rat-cli/tests/mcp_hot_upgrade.rs:75+` — build a temp repo with `rag-rat.toml`, run `rag-rat index`, then spawn `rag-rat mcp` exactly as `spawn_mcp` does and drive `initialize` so the server is fully up)

```rust
#[cfg(unix)]
#[test]
fn socket_path_serves_dedupes_and_falls_back() {
    // 1. temp repo: rag-rat.toml + a source file mentioning `frobnicate`; run `rag-rat index`.
    // 2. spawn `rag-rat mcp` (same harness as mcp_hot_upgrade TestEnv) and wait for the
    //    socket file (poll <db parent>/sockets/*.sock, 10s deadline).
    // 3. run_hook() with a Grep tool_input {"pattern": "frobnicate"}, cwd = repo root:
    //    stdout JSON contains "additionalContext" mentioning the indexed file.
    // 4. same invocation again (same session_id): stdout is EMPTY (listener deduped to null).
    // 5. kill the mcp process; same invocation: stdout has additionalContext again
    //    (stateless fallback, no dedupe) — proves the direct-SQLite path.
    // 6. different session_id while server was alive would get context (cover via step 3'
    //    with session_id "s2" before killing if convenient).
}
```

Write this as real code: every step above is concrete — `run_hook` exists from Task 10, the spawn/initialize harness is copy-adapted from `mcp_hot_upgrade.rs` (`TestEnv::setup`, `spawn_mcp`, `initialize`). The only new helper is polling for the socket file under `<repo>/.rag-rat/sockets/`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p rag-rat-cli --test claude_hook_e2e socket_path_serves`
Expected: FAIL at step 3 or 4 if any wiring is missing; PASS only when listener, client, dedupe, and fallback all work. (If it passes immediately, verify it actually exercised the socket: temporarily assert the socket file exists.)

- [ ] **Step 3: Fix whatever the e2e surfaces** — likely candidates: socket dir creation, the listener's election losing to a stale lock from a previous test (use unique temp roots per test, as `mcp_hot_upgrade.rs` does), timing (poll, don't sleep).

- [ ] **Step 4: Full suite**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add crates/rag-rat-cli/tests/claude_hook_e2e.rs
git commit -m "test(cli): end-to-end socket + dedupe + fallback coverage for claude-hook"
```

---

### Task 13: Hot-upgrade interaction — socket survives handoff

**Files:**
- Modify: `crates/rag-rat-cli/tests/mcp_hot_upgrade.rs`

- [ ] **Step 1: Extend the existing resume test**

In `sigusr1_hot_upgrade_resumes_session_in_place` (mcp_hot_upgrade.rs:28), after the post-upgrade `call_semantic_search` assertion, add a hook-socket probe:

```rust
    // The grep-augment hook socket must answer after the in-place exec: the new process
    // re-runs the socket election (the old lock fd died with the exec'd image) and re-binds.
    let socket = find_hook_socket(&env.root);
    let reply = hook_roundtrip(&socket, "sqlite");
    assert_eq!(reply["v"], 1, "hook socket answers after hot-upgrade");
```

With test-local helpers:

```rust
fn find_hook_socket(root: &Path) -> PathBuf {
    let sockets_dir = root.join(".rag-rat/sockets");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(mut entries) = fs::read_dir(&sockets_dir) {
            if let Some(Ok(entry)) = entries.next() {
                return entry.path();
            }
        }
        assert!(Instant::now() < deadline, "hook socket never appeared in {sockets_dir:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn hook_roundtrip(socket: &Path, pattern: &str) -> Value {
    use std::os::unix::net::UnixStream;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(stream) = UnixStream::connect(socket) {
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut writer = stream.try_clone().unwrap();
            let request = json!({"v": 1, "kind": "grep_augment", "session_id": "upgrade-probe",
                                 "pattern": pattern, "search_path": null, "source": "grep_tool"});
            writeln!(writer, "{request}").unwrap();
            let mut line = String::new();
            if BufReader::new(stream).read_line(&mut line).is_ok() && !line.is_empty() {
                return serde_json::from_str(&line).unwrap();
            }
        }
        assert!(Instant::now() < deadline, "hook socket never answered after upgrade");
        std::thread::sleep(Duration::from_millis(100));
    }
}
```

(Adjust the DB-parent path if `TestEnv` configures the database elsewhere — read its `rag-rat.toml` to locate `database` rather than assuming `.rag-rat/`.)

- [ ] **Step 2: Run** — `cargo test -p rag-rat-cli --test mcp_hot_upgrade` Expected: PASS. If the post-exec process never re-binds, the bug is in Task 8 (listener spawned only on the cold path — ensure `spawn_listener` runs in **both** the cold-start and handoff-resume branches of `run_stdio_unix`).

- [ ] **Step 3: Commit**

```bash
git add crates/rag-rat-cli/tests/mcp_hot_upgrade.rs
git commit -m "test(mcp): hook socket re-binds after SIGUSR1 hot-upgrade"
```

---

### Task 14: Docs

**Files:**
- Modify: `README.md` (add a "Claude Code grep augmentation" section near the existing hooks/MCP docs)
- Modify: `CLAUDE.md` (one line in the MCP-preference section noting the hook exists and is installed via `rag-rat hooks install --claude`)

- [ ] **Step 1: Write the README section** — cover: what it does (augments Grep/Bash grep/rg/ag with symbols + memories via `additionalContext`), install (`rag-rat hooks install --claude`, `--global` variant and its no-op safety), how it serves (elected socket per worktree, read-only fallback, never blocks), dedupe semantics (per session; stateless on fallback), and `RAG_RAT_HOOK_DEBUG=1` troubleshooting.

- [ ] **Step 2: Verify docs claims against the implementation** — every command and path named in the section must exist (`rag-rat hooks status --claude` output shape, socket location, timeout values).

- [ ] **Step 3: Commit**

```bash
git add README.md CLAUDE.md
git commit -m "docs: grep-augmentation PreToolUse hook usage and install"
```

---

## Final verification (before finishing-a-development-branch)

```bash
cargo fmt --check
cargo clippy --all-targets   # deny-warnings posture
cargo test                   # full workspace
```

Then dogfood: in the worktree, `rag-rat index --discover && rag-rat hooks install --claude`, open a Claude Code session, grep for a known symbol, and confirm the injected context appears (and doesn't on the second grep).
