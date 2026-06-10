# Harden Repo-Memory Anchoring — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.
>
> Spec: `docs/plans/2026-06-10-harden-memory-anchoring-design.md`. This plan doc and the spec stay **uncommitted** (repo convention). The per-task `git commit` steps commit the **implementation code**, not these docs.

**Goal:** Make repo memories survive cross-file symbol/chunk moves (content-confirmed auto-relocation), add a first-class `memory_rebind`, and surface non-current anchors so they never rot silently.

**Architecture:** Three phases. Phase 1 extends the re-anchor fallback in `query/memory/validate.rs` to find a moved symbol by bare name + `source_text_hash`, persist the rewritten `binding_id`, and corroborate with two new durable binding columns (`symbol_kind`, `signature_hash`). Phase 2 adds `rebind_memory` + MCP tool + CLI. Phase 3 adds `rag-rat memory doctor`, post-index warnings, and `index_status` anchor-health counts.

**Tech Stack:** Rust 2024, rusqlite (SQLite), serde, sha2. Workspace crates: `rag-rat-core` (engine), `rag-rat-mcp` (MCP server), `rag-rat-cli` (`rag-rat` binary).

**Verification per task:** `cargo test -p rag-rat-core <name>`, then at phase end `cargo clippy --workspace --all-targets` clean and `cargo +nightly fmt --all`. Tests live inline in `crates/rag-rat-core/src/index/schema_bootstrap_tests.rs` (integration-style, build a temp index) and `query/memory/validate.rs` (`#[cfg(test)] mod`).

---

## File structure

| File | Responsibility | Change |
|---|---|---|
| `crates/rag-rat-core/src/index/schema/mod.rs` | migration registry | add `MIGRATION_014_*` consts, bump `LATEST_SCHEMA_VERSION`, wire `apply()` |
| `crates/rag-rat-core/src/index/schema/migrations.rs` | migration bodies | `apply_memory_binding_signals` + version/known/checksum arms |
| `crates/rag-rat-core/src/query/memory/mod.rs` | DTOs | add 2 fields to `RepoMemoryBinding` + `ResolvedBinding` |
| `crates/rag-rat-core/src/query/memory/resolve.rs` | bind resolution + row mapping + persist helpers | populate new signals; `binding_row`/`insert_binding`; new `relocate_*` + `symbol_signal` helpers |
| `crates/rag-rat-core/src/query/memory/validate.rs` | re-anchor logic | bare-name + hash fallback for symbol/logical/chunk |
| `crates/rag-rat-core/src/query/memory/api.rs` | top-level ops | persist `binding_id` rewrite + new columns; transaction; `rebind_memory` |
| `crates/rag-rat-core/src/index/query_api.rs` | `IndexDatabase` facade | `memory_rebind`, `memory_doctor` wrappers |
| `crates/rag-rat-mcp/src/tools/{args,handlers,mod}.rs` | MCP surface | `memory_rebind` tool |
| `crates/rag-rat-cli/src/commands.rs` (+ `main.rs` dispatch) | CLI | `memory rebind`, `memory doctor` |
| `crates/rag-rat-core/src/index/schema_bootstrap_tests.rs` | tests | relocation + rebind + doctor cases |

---

## Phase 1 — Engine self-heal

### Task 1: Additive migration for binding signal columns

**Files:**
- Modify: `crates/rag-rat-core/src/index/schema/mod.rs`
- Modify: `crates/rag-rat-core/src/index/schema/migrations.rs`

- [ ] **Step 1: Add the migration constants** in `schema/mod.rs`, immediately after the `MIGRATION_013_*` consts:

```rust
const MIGRATION_014_ID: &str = "014_repo_memory_binding_signals";
const MIGRATION_014_CHECKSUM: &str = "sha256:rag-rat-repo-memory-binding-signals-v14";
const MIGRATION_014_DESCRIPTION: &str =
    "Add symbol_kind + signature_hash to repo_memory_bindings for durable cross-file relocation";
```

- [ ] **Step 2: Bump the version** in `schema/mod.rs`:

```rust
pub const LATEST_SCHEMA_VERSION: u32 = 14;
```

- [ ] **Step 3: Wire `apply()`** in `schema/mod.rs`. Find the block that ends the migration sequence (after the `migrate`/`apply_*` call for 013 and its `record_migration(conn, MIGRATION_013_ID, ...)`). Add directly after it:

```rust
    migrations::apply_memory_binding_signals(conn)?;
    record_migration(conn, MIGRATION_014_ID, MIGRATION_014_CHECKSUM, MIGRATION_014_DESCRIPTION)?;
```

(Match the exact call/qualification style used for the 013 line right above it — if the file calls the bare fn name via `use`, do the same.)

- [ ] **Step 4: Add the migration body** in `migrations.rs`, following the `apply_*` pattern (e.g. `apply_graph_file_lookup_indexes`):

```rust
pub(crate) fn apply_memory_binding_signals(conn: &Connection) -> rusqlite::Result<()> {
    // Durable corroboration signals for cross-file relocation: a moved symbol keeps its
    // kind + signature even when its path-qualified name (and rowids) change.
    add_column_if_missing(conn, "repo_memory_bindings", "symbol_kind", "TEXT")?;
    add_column_if_missing(conn, "repo_memory_bindings", "signature_hash", "TEXT")?;
    Ok(())
}
```

- [ ] **Step 5: Register the id** in `migrations.rs` — add an arm to `known_version` (returns `Option<u32>`):

```rust
            MIGRATION_014_ID => Some(14),
```

to `known_migration`:

```rust
            | MIGRATION_014_ID
```

and to `migration_checksum_mismatch`:

```rust
        MIGRATION_014_ID => migration.checksum != MIGRATION_014_CHECKSUM,
```

- [ ] **Step 6: Build + verify a fresh index applies cleanly**

Run: `cargo build -p rag-rat-core`
Expected: compiles. Then run the existing schema test:
Run: `cargo test -p rag-rat-core schema_bootstrap_tests::compatible_open_requires_recorded_schema_version`
Expected: PASS (version now 14 end-to-end).

- [ ] **Step 7: Commit**

```bash
git add crates/rag-rat-core/src/index/schema/mod.rs crates/rag-rat-core/src/index/schema/migrations.rs
git commit -m "feat(schema): V014 add symbol_kind + signature_hash to repo_memory_bindings"
```

### Task 2: Carry the new columns through the DTOs and row I/O

**Files:**
- Modify: `crates/rag-rat-core/src/query/memory/mod.rs` (`RepoMemoryBinding`, `ResolvedBinding`)
- Modify: `crates/rag-rat-core/src/query/memory/resolve.rs` (`binding_row`, `insert_binding`)
- Modify: `crates/rag-rat-core/src/query/memory/api.rs` (the two SELECT column lists + persist UPDATE)

- [ ] **Step 1: Add fields to `RepoMemoryBinding`** (`mod.rs`), after `github_number`:

```rust
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_hash: Option<String>,
```

- [ ] **Step 2: Add fields to `ResolvedBinding`** (`mod.rs`), after `github_number`:

```rust
    symbol_kind: Option<String>,
    signature_hash: Option<String>,
```

- [ ] **Step 3: Set the new fields in every `ResolvedBinding { .. }` literal in `resolve.rs`.** For the non-symbol literals (`resolve_chunk_binding`, `resolve_edge_binding`, `resolve_call_path_binding`, `resolve_path_binding`, the inline commit/github literals in `resolve_binding`) add:

```rust
        symbol_kind: None,
        signature_hash: None,
```

For `resolve_symbol_binding` and `resolve_logical_symbol_binding`, compute them (see Step 4) and set `symbol_kind: kind, signature_hash: sig_hash,`.

- [ ] **Step 4: Add a `symbol_signal` helper** in `resolve.rs`:

```rust
pub(crate) fn symbol_signal(
    conn: &Connection,
    symbol_id: i64,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    let row = conn
        .query_row(
            "SELECT kind, signature FROM symbols WHERE id = ?1",
            [symbol_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    Ok(match row {
        Some((kind, signature)) => {
            (Some(kind), signature.map(|sig| hex_sha256(sig.trim().as_bytes())))
        }
        None => (None, None),
    })
}
```

In `resolve_symbol_binding`, after `let symbol = ...lookup_by_id...`, add `let (kind, sig_hash) = symbol_signal(conn, symbol_id)?;` and use them in the literal. In `resolve_logical_symbol_binding`, derive the member symbol id from the resolved `chunk` (`chunk.as_ref().and_then(|c| c.symbol_id)`) and call `symbol_signal` when present, else `(None, None)`.

- [ ] **Step 5: Extend `insert_binding`** (`resolve.rs`) — add the two columns + placeholders to the `INSERT INTO repo_memory_bindings(...)` and the `params![...]` (after `github_number`, before `anchor_status`):

```rust
            // columns:  ... github_number, symbol_kind, signature_hash, anchor_status, created_at_ms
            // values:   ... ?14, ?15, ?16, ?17, ?18
```

i.e. insert `binding.symbol_kind, binding.signature_hash` into the params list in column order and renumber the trailing placeholders.

- [ ] **Step 6: Extend `binding_row`** (`resolve.rs`):

```rust
        symbol_kind: row.get("symbol_kind")?,
        signature_hash: row.get("signature_hash")?,
```

- [ ] **Step 7: Add the columns to both SELECTs that feed `binding_row`** — `attach_memory_children` (`resolve.rs`) and `validate_memories` (`api.rs`): add `symbol_kind, signature_hash` to the column list (anywhere before `anchor_status` is fine; `binding_row` reads by name).

- [ ] **Step 8: Build**

Run: `cargo build -p rag-rat-core`
Expected: compiles (no behavior change yet; columns now round-trip).

- [ ] **Step 9: Commit**

```bash
git add crates/rag-rat-core/src/query/memory/
git commit -m "feat(memory): persist symbol_kind + signature_hash on bindings"
```

### Task 3: Persist the rewritten `binding_id` (+ new columns) on relocation

**Files:**
- Modify: `crates/rag-rat-core/src/query/memory/api.rs` (`validate_memories`)

This is the durability fix (P1.1): the persist `UPDATE` must match the OLD `binding_id` and SET the new one, plus the new columns, inside one transaction.

- [ ] **Step 1: Capture the original binding_id and wrap in a transaction.** Replace the body of `validate_memories` row loop. Before the loop:

```rust
    let tx = conn.unchecked_transaction()?;
```

Inside the loop, before `validate_binding`:

```rust
        let mut binding = row?;
        let original_binding_id = binding.binding_id.clone();
        report.checked += 1;
        let status = validate_binding(conn, &mut binding)?;
```

- [ ] **Step 2: Rewrite the persist `UPDATE`** to set `binding_id`, `symbol_kind`, `signature_hash`, and match on the original id:

```rust
        let updated = conn.execute(
            "
            UPDATE OR IGNORE repo_memory_bindings
            SET anchor_status = ?3, logical_symbol_id = ?4, symbol_id = ?5, chunk_id = ?6,
                edge_id = ?7, path = ?8, start_line = ?9, end_line = ?10,
                binding_id = ?11, symbol_kind = ?12, signature_hash = ?13
            WHERE memory_id = ?1 AND binding_kind = ?2 AND binding_id = ?14
            ",
            params![
                binding.memory_id, binding.binding_kind, status,
                binding.logical_symbol_id, binding.symbol_id, binding.chunk_id, binding.edge_id,
                binding.path, binding.start_line, binding.end_line,
                binding.binding_id, binding.symbol_kind, binding.signature_hash,
                original_binding_id
            ],
        )?;
        // UPDATE OR IGNORE: if a sibling binding already holds the new (memory_id, kind, binding_id)
        // PK, the rewrite is a no-op rather than a crash. Drop the now-duplicate stale row.
        if updated == 0 && binding.binding_id != original_binding_id {
            conn.execute(
                "DELETE FROM repo_memory_bindings
                 WHERE memory_id = ?1 AND binding_kind = ?2 AND binding_id = ?3",
                params![binding.memory_id, binding.binding_kind, original_binding_id],
            )?;
        }
```

After the loop, before `Ok(report)`:

```rust
    tx.commit()?;
```

(Note: `validate_binding` does NOT yet rewrite `binding_id`; Task 4 makes it do so. This task makes the persist layer correct for when it does, and is verified by Task 5's regression test.)

- [ ] **Step 3: Build**

Run: `cargo build -p rag-rat-core`
Expected: compiles. Existing tests still pass:
Run: `cargo test -p rag-rat-core repo_memory`
Expected: PASS (no relocation behavior change yet).

- [ ] **Step 4: Commit**

```bash
git add crates/rag-rat-core/src/query/memory/api.rs
git commit -m "fix(memory): persist binding_id rewrite + signals in one txn on re-anchor"
```

### Task 4: Bare-name + content-hash relocation fallback

**Files:**
- Modify: `crates/rag-rat-core/src/query/memory/validate.rs`
- Modify: `crates/rag-rat-core/src/query/memory/resolve.rs` (candidate query helper)

- [ ] **Step 1: Add a candidate-relocation helper** in `resolve.rs`. It searches by bare name, context-scoped via the `files` join (matching the existing fallback), and returns the unique content-hash match:

```rust
pub(crate) struct RelocateMatch {
    pub binding_id: String,
    pub symbol_id: i64,
    pub logical_symbol_id: Option<i64>,
    pub path: String,
    pub chunk_id: Option<i64>,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub symbol_kind: Option<String>,
    pub signature_hash: Option<String>,
}

/// Find the unique moved home of a symbol whose stored anchor is gone.
/// `short_name` = the symbol name with its old `"{path}::"` prefix stripped.
/// Relocation requires a content-hash match (chunk.text_hash == source_text_hash);
/// kind/signature corroborate, never override. Returns Some only when exactly one
/// candidate content-matches.
pub(crate) fn relocate_symbol_by_name(
    conn: &Connection,
    short_name: &str,
    source_text_hash: &str,
) -> anyhow::Result<Option<RelocateMatch>> {
    let mut stmt = conn.prepare(
        "
        SELECT symbols.id AS symbol_id, symbols.qualified_name AS qualified_name,
               files.path AS path, symbols.kind AS kind, symbols.signature AS signature
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        WHERE symbols.name = ?1
        ",
    )?;
    let rows = stmt.query_map([short_name], |row| {
        Ok((
            row.get::<_, i64>("symbol_id")?,
            row.get::<_, String>("qualified_name")?,
            row.get::<_, String>("path")?,
            row.get::<_, String>("kind")?,
            row.get::<_, Option<String>>("signature")?,
        ))
    })?;
    let mut matched: Option<RelocateMatch> = None;
    for row in rows {
        let (symbol_id, qualified_name, path, kind, signature) = row?;
        let chunk = chunk_for_symbol(conn, symbol_id, &qualified_name)?;
        let text_hash = chunk.as_ref().map(|c| c.text_hash.as_str());
        if text_hash != Some(source_text_hash) {
            continue; // content-hash is required for a silent relocate
        }
        if matched.is_some() {
            return Ok(None); // >=2 content matches -> ambiguous -> stay gone
        }
        matched = Some(RelocateMatch {
            binding_id: qualified_name,
            symbol_id,
            logical_symbol_id: logical_symbol_id_for_symbol(conn, symbol_id)?,
            path,
            chunk_id: chunk.as_ref().map(|c| c.chunk_id),
            start_line: chunk.as_ref().map(|c| c.start_line),
            end_line: chunk.as_ref().map(|c| c.end_line),
            symbol_kind: Some(kind),
            signature_hash: signature.map(|s| hex_sha256(s.trim().as_bytes())),
        });
    }
    Ok(matched)
}

/// Strip the persisted `"{path}::"` prefix from a path-qualified binding_id.
/// Falls back to last-`::` split only when path is absent.
pub(crate) fn short_symbol_name<'a>(binding_id: &'a str, path: Option<&str>) -> &'a str {
    if let Some(path) = path
        && let Some(rest) = binding_id.strip_prefix(path)
        && let Some(name) = rest.strip_prefix("::")
    {
        return name;
    }
    binding_id.rsplit("::").next().unwrap_or(binding_id)
}
```

- [ ] **Step 2: Use it in `validate_symbol_binding`** (`validate.rs`). Replace the `let Some((id, path)) = relocated else { return Ok("gone"...) };` tail (the qualified_name fallback) so that, when the qualified_name lookup misses, it tries the name+hash relocation before giving up:

```rust
    // (existing exact-id check stays above)
    // (existing qualified_name lookup stays; on hit it still returns "relocated")
    if let Some((id, path)) = relocated {
        binding.symbol_id = Some(id);
        binding.logical_symbol_id = logical_symbol_id_for_symbol(conn, id)?;
        binding.path = Some(path);
        if let Some(chunk) = chunk_for_symbol(conn, id, &binding.binding_id)? {
            binding.chunk_id = Some(chunk.chunk_id);
            binding.start_line = Some(chunk.start_line);
            binding.end_line = Some(chunk.end_line);
        }
        let (kind, sig) = symbol_signal(conn, id)?;
        binding.symbol_kind = kind;
        binding.signature_hash = sig;
        return Ok("relocated".to_string());
    }
    // Cross-file move: qualified_name changed with the path. Match by bare name + content hash.
    if let Some(hash) = source_hash_for_memory(conn, &binding.memory_id)? {
        let short = short_symbol_name(&binding.binding_id, binding.path.as_deref()).to_string();
        if let Some(m) = relocate_symbol_by_name(conn, &short, &hash)? {
            binding.binding_id = m.binding_id;
            binding.symbol_id = Some(m.symbol_id);
            binding.logical_symbol_id = m.logical_symbol_id;
            binding.path = Some(m.path);
            binding.chunk_id = m.chunk_id;
            binding.start_line = m.start_line;
            binding.end_line = m.end_line;
            binding.symbol_kind = m.symbol_kind;
            binding.signature_hash = m.signature_hash;
            return Ok("relocated".to_string());
        }
    }
    Ok("gone".to_string())
```

(Apply the analogous change to `validate_logical_symbol_binding`: after the `qualified_name`-in-`logical_symbols` miss, run the same `source_hash` + `relocate_symbol_by_name` path, then set `logical_symbol_id = relocate's logical_symbol_id` and rewrite `binding_id` to the relocated member's qualified_name. Keep the existing logical lookup arm intact.)

- [ ] **Step 3: Build**

Run: `cargo build -p rag-rat-core`
Expected: compiles.

- [ ] **Step 4: Commit**

```bash
git add crates/rag-rat-core/src/query/memory/
git commit -m "feat(memory): relocate symbol bindings across files by name + content hash"
```

### Task 5: Tests for cross-file relocation + durability

**Files:**
- Modify: `crates/rag-rat-core/src/index/schema_bootstrap_tests.rs`

Use the existing `repo_memory_*` tests as the template for fixture setup (build a temp index, create a memory bound to a symbol, edit files, `IndexDatabase::open(...).index_*` / rebuild, then `memory_validate`). Mirror their helpers (`unique_temp_root`, `source_config`).

- [ ] **Step 1: Write the failing tests.** Add (filling in fixture setup exactly like the neighbouring `repo_memory_survives_reindex_and_relocates_when_symbol_moves` test):

```rust
#[test]
fn memory_relocates_when_symbol_moves_to_another_file() {
    // bind a memory to `fn target` in a.rs; move `fn target` verbatim to b.rs; reindex.
    // expect: validate report relocated == 1, gone == 0; binding.path now b.rs.
}

#[test]
fn memory_relocation_is_durable_across_a_second_reindex() {
    // after the move+relocate above, edit b.rs (unrelated line) and reindex again.
    // expect: status resolves via the rewritten qualified_name (current/stale), NOT gone,
    // and the relocation fallback is not re-entered (binding_id == new qualified_name).
}

#[test]
fn memory_stays_gone_when_moved_symbol_body_changed() {
    // move `fn target` to b.rs AND edit its body (hash differs). reindex.
    // expect: gone == 1 (no silent relocate).
}

#[test]
fn memory_stays_gone_when_two_files_define_the_same_name() {
    // two files define `fn target` with identical bodies; bound one is deleted.
    // expect: gone (>=2 content matches -> ambiguous -> gone), not a wrong relocate.
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p rag-rat-core memory_relocat`
Expected: the move/durability tests FAIL pre-fix behavior would be `gone` — confirm they now exercise the new path (they should PASS after Task 4; if Task 4 is already in, instead assert they pass and the pre-Task-4 git stash fails).

- [ ] **Step 3: Make them pass / confirm pass**

Run: `cargo test -p rag-rat-core memory_relocat memory_stays_gone memory_relocation`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/rag-rat-core/src/index/schema_bootstrap_tests.rs
git commit -m "test(memory): cross-file relocation, durability, ambiguity guards"
```

### Task 6: Chunk-binding content-exact relocation

**Files:**
- Modify: `crates/rag-rat-core/src/query/memory/validate.rs` (`validate_chunk_binding`)
- Modify: `crates/rag-rat-core/src/query/memory/resolve.rs` (chunk-by-hash helper)
- Modify: `crates/rag-rat-core/src/index/schema_bootstrap_tests.rs` (test)

- [ ] **Step 1: Add `relocate_chunk_by_hash`** in `resolve.rs`:

```rust
pub(crate) fn relocate_chunk_by_hash(
    conn: &Connection,
    source_text_hash: &str,
) -> anyhow::Result<Option<ChunkAnchor>> {
    let mut stmt = conn.prepare(
        "
        SELECT chunks.id AS chunk_id, files.path AS path, chunks.start_line AS start_line,
               chunks.end_line AS end_line, chunks.symbol_path AS symbol_path,
               chunks.text_hash AS text_hash, NULL AS symbol_id
        FROM chunks JOIN files ON files.id = chunks.file_id
        WHERE chunks.text_hash = ?1
        ",
    )?;
    let mut rows = stmt.query_map([source_text_hash], chunk_anchor_row)?;
    let Some(first) = rows.next() else { return Ok(None) };
    let first = first?;
    if rows.next().is_some() {
        return Ok(None); // >=2 -> ambiguous -> stay gone
    }
    Ok(Some(first))
}
```

- [ ] **Step 2: Extend `validate_chunk_binding`** (`validate.rs`) — currently it delegates to `validate_bound_chunk` which returns `gone` when the stored `chunk_id` row is missing. Add a fallback:

```rust
pub(crate) fn validate_chunk_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    let status = validate_bound_chunk(conn, binding)?;
    if status != "gone" {
        return Ok(status);
    }
    let Some(hash) = source_hash_for_memory(conn, &binding.memory_id)? else {
        return Ok("gone".to_string());
    };
    let Some(chunk) = relocate_chunk_by_hash(conn, &hash)? else {
        return Ok("gone".to_string());
    };
    binding.binding_id = chunk.chunk_id.to_string();
    binding.chunk_id = Some(chunk.chunk_id);
    binding.path = Some(chunk.path);
    binding.start_line = Some(chunk.start_line);
    binding.end_line = Some(chunk.end_line);
    Ok("relocated".to_string())
}
```

- [ ] **Step 3: Test + run + commit**

Add `memory_chunk_binding_relocates_by_hash` to `schema_bootstrap_tests.rs`.
Run: `cargo test -p rag-rat-core memory_chunk_binding`
Expected: PASS.

```bash
git add crates/rag-rat-core/src/query/memory/ crates/rag-rat-core/src/index/schema_bootstrap_tests.rs
git commit -m "feat(memory): content-exact relocation for chunk bindings"
```

### Task 7: Phase-1 gate

- [ ] **Step 1:** `cargo clippy --workspace --all-targets` → no warnings (widen any private types newly used in `pub(crate)` signatures to `pub(crate)`, per the prior sweep's pattern).
- [ ] **Step 2:** `cargo test -p rag-rat-core` → baseline + new tests pass.
- [ ] **Step 3:** `cargo +nightly fmt --all`; commit any formatting: `git commit -am "style: fmt"`.

---

## Phase 2 — `memory_rebind`

### Task 8: `rebind_memory` core

**Files:**
- Modify: `crates/rag-rat-core/src/query/memory/api.rs`
- Modify: `crates/rag-rat-core/src/index/query_api.rs` (facade)
- Test: `crates/rag-rat-core/src/index/schema_bootstrap_tests.rs`

- [ ] **Step 1: Write the failing test** `memory_rebind_reanchors_and_refreshes_hash` — create a memory bound to a symbol, move the symbol so it goes `gone`, then `rebind_memory` to the new symbol id; assert the returned binding `anchor_status == "current"` and the memory's `source_text_hash` equals the new chunk's hash (no stale flap on a follow-up `memory_validate`).

- [ ] **Step 2: Implement `rebind_memory`** in `api.rs`:

```rust
pub(crate) fn rebind_memory(
    conn: &Connection,
    memory_id: &str,
    bind: RepoMemoryBindTarget,
) -> anyhow::Result<RepoMemory> {
    if memory_by_id(conn, memory_id)?.is_none() {
        anyhow::bail!("memory `{memory_id}` not found");
    }
    let binding = resolve_binding(conn, &bind)?;
    let tx = conn.unchecked_transaction()?;
    conn.execute("DELETE FROM repo_memory_bindings WHERE memory_id = ?1", [memory_id])?;
    conn.execute("DELETE FROM repo_memory_call_paths WHERE memory_id = ?1", [memory_id])?;
    let now = now_ms();
    insert_binding(conn, memory_id, &binding, now)?;
    conn.execute(
        "UPDATE repo_memories SET source_text_hash = ?2, updated_at_ms = ?3 WHERE id = ?1",
        params![memory_id, binding.source_text_hash, now],
    )?;
    tx.commit()?;
    memory_by_id(conn, memory_id)?
        .ok_or_else(|| anyhow::anyhow!("rebound memory `{memory_id}` could not be read back"))
}
```

- [ ] **Step 3: Add facade** `IndexDatabase::memory_rebind(&self, memory_id, bind)` in `query_api.rs`, mirroring the existing `memory_update`/`memory_create` wrappers (same `with_*_conn`/`self.conn` access pattern as its neighbours).

- [ ] **Step 4:** Run `cargo test -p rag-rat-core memory_rebind` → PASS. Commit.

### Task 9: MCP tool + CLI for rebind

**Files:**
- Modify: `crates/rag-rat-mcp/src/tools/args.rs`, `handlers.rs`, `mod.rs`
- Modify: `crates/rag-rat-cli/src/commands.rs` (+ `main.rs` subcommand dispatch + `usage()`)

- [ ] **Step 1: MCP.** Register `memory_rebind` in the **same three sites** `memory_create` is registered (grep `memory_create` in `crates/rag-rat-mcp/src/tools/`): (a) add `"memory_rebind"` to `TOOL_NAMES` (`tools/mod.rs`); (b) add a `MemoryRebindArgs { memory_id: String, bind: MemoryBindArgs }` struct in `args.rs` (reuse the existing `MemoryBindArgs` + its `From<MemoryBindArgs> for RepoMemoryBindTarget`); (c) add the `call_tool_with_db` dispatch arm + a `description()` + `schema()` entry in `handlers.rs` calling `db.memory_rebind(args.memory_id, args.bind.into())`.

- [ ] **Step 2: CLI.** In `cli/commands.rs` add a `rebind` branch to the `memory` subcommand (find the existing `memory create`/`memory search` dispatch), parsing `<memory_id>` + one of `--symbol <name>` / `--path <p>` / `--chunk <id>`. For `--symbol`, resolve the name to an id via the existing symbol lookup (`db.select_symbol`/`symbol_candidates`), then call `db.memory_rebind`. Add a usage line in `main.rs::usage()`.

- [ ] **Step 3:** `cargo test --workspace` (incl. `crates/rag-rat-cli/tests/mcp_stdio.rs` if it enumerates tools — update the expected tool list), `cargo clippy --workspace --all-targets`. Commit.

---

## Phase 3 — Surfacing

### Task 10: `memory doctor`

**Files:**
- Modify: `crates/rag-rat-core/src/query/memory/api.rs` (a `doctor_report`)
- Modify: `crates/rag-rat-core/src/index/query_api.rs` (facade)
- Modify: `crates/rag-rat-cli/src/commands.rs` + `main.rs`
- Test: `schema_bootstrap_tests.rs`

- [ ] **Step 1: Core report.** Add to `api.rs`:

```rust
#[derive(Debug, Clone, Serialize)]
pub struct MemoryDoctorEntry {
    pub memory_id: String,
    pub title: String,
    pub binding_kind: String,
    pub binding_id: String,
    pub anchor_status: String,
    pub candidates: Vec<String>, // qualified_names of live same-name symbols, ranked
}

pub(crate) fn doctor_report(conn: &Connection) -> anyhow::Result<Vec<MemoryDoctorEntry>> {
    // Select active memories with any binding whose anchor_status IN ('gone','stale'),
    // join repo_memories (status='active'); for each, recompute live candidates via a
    // bare-name search ranked by kind + signature_hash agreement (reuse short_symbol_name).
    // ... (query mirrors validate_memories' SELECT; per-row candidate recompute) ...
    todo!("implemented in step 2")
}
```

- [ ] **Step 2: Implement** `doctor_report`: query bindings where `anchor_status IN ('gone','stale')` joined to active memories; for each symbol/logical binding, compute `short_symbol_name` and run a name search (same query as `relocate_symbol_by_name` minus the hash filter), ranking candidates whose `kind`/`signature_hash` match the binding's stored `symbol_kind`/`signature_hash` first. Return the entries.

- [ ] **Step 3: Facade + CLI.** `IndexDatabase::memory_doctor()` in `query_api.rs`; `rag-rat memory doctor` in `cli/commands.rs` prints each entry, the suggested `rag-rat memory rebind <id> --symbol <candidate>` line, and for zero-candidate `gone` entries prints `→ code appears deleted; rag-rat memory mark-obsolete <id>`. Exit non-zero if any `gone` remains.

- [ ] **Step 4: Test** `memory_doctor_lists_gone_and_suggests_candidates` + exit-code behavior. Run, commit.

### Task 11: Post-index warning + `index_status` counts

**Files:**
- Modify: `crates/rag-rat-cli/src/commands.rs` (post-index/reconcile notice)
- Modify: `crates/rag-rat-mcp/src/...` `index_status` handler + the core status struct it serializes

- [ ] **Step 1:** After `index`/`reconcile` complete in the CLI, call `memory_validate` (already run by reindex) results or `doctor_report`; if any active memory is non-current, print `⚠ N repo memories need re-anchoring — run 'rag-rat memory doctor'` to stderr.
- [ ] **Step 2:** Extend the `index_status` payload with an `anchor_health: { current, relocated, stale, gone }` object over active memories (reuse `RepoMemoryValidationReport` shape via a read-only count query — do not mutate during status).
- [ ] **Step 3:** Test + commit.

### Task 12: Final gate

- [ ] `cargo clippy --workspace --all-targets` clean; `cargo test --workspace` green; `cargo +nightly fmt --all`; update `crates/rag-rat-cli/tests/mcp_stdio.rs` tool list if needed. Commit.

---

## Self-review notes

- Spec coverage: Phase 1a→Task 1; 1b→Tasks 2,4; 1c→Task 6; 1d (consumer audit/txn)→Task 3 + Task 7; Phase 2→Tasks 8-9; Phase 3→Tasks 10-11. All spec sections mapped.
- The only `todo!()` placeholder (Task 10 Step 1) is resolved in Task 10 Step 2 — keep them adjacent.
- Names used consistently: `relocate_symbol_by_name`, `short_symbol_name`, `symbol_signal`, `relocate_chunk_by_hash`, `rebind_memory`, `doctor_report`, `memory_rebind`, `memory_doctor`.
- Migration is additive (`add_column_if_missing`), nullable columns — safe for existing DBs; no `ambiguous` status added, so no `anchor_status` consumer changes beyond the relocation rewrites.
