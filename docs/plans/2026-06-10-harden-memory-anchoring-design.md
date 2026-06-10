# Design — Harden repo-memory anchoring

Status: design approved (revised after Fable 5 review), uncommitted — for review.
Left on disk per the repo convention of not committing plan/spec docs.

## Problem

Repo memories bind to code by symbol / logical-symbol / chunk / edge / path. On
reindex `memory_validate` re-anchors each binding and marks it
`current | relocated | stale | gone | unverified`. Re-anchoring only survives
**in-file** movement; a **cross-file** symbol move detaches the memory (`gone`),
and path/chunk bindings to a moved/split/edited file are always `gone`.

Observed during the god-module split sweep (commit d582c01): of 5 active memories,
3 went `gone` and 1 `stale`, `relocated` caught 0. Every symbol that moved files
(`rebuild_logical_symbols` → `internals.rs`, `rebuild_with_progress` →
`rebuild.rs`) and the path-bound `index/ai.rs` note detached, and had to be
re-created by hand — which also surfaced a **duplicate trap**: `memory_create`
with identical content returns `duplicate: true` and does not rebind, so there is
no clean re-anchor path.

### Root cause

`validate_symbol_binding` / `validate_logical_symbol_binding`
(`crates/rag-rat-core/src/query/memory/validate.rs`): when the stored `symbol_id`
no longer resolves, the only fallback is
`SELECT ... FROM symbols WHERE qualified_name = ?1` with `?1 = binding.binding_id`.
`qualified_name` is `format!("{path}::{name}")` (`index/parser.rs:321`), so it
embeds the file path; after a cross-file move the new qualified_name differs and
the lookup misses → `gone`. `validate_path_binding` and `validate_chunk_binding`
have no relocation fallback at all.

### Durable vs volatile identifiers (corrected premise)

- `symbol_id` / `chunk_id` are rowids reassigned **when their file is
  re-indexed** (per-file delete+reinsert, `index/internals.rs`) or on full
  rebuild — not on every pass, but on any edit to the owning file.
- `logical_symbol_id` is a deterministic `LogicalSymbolKey::stable_id()` — a
  63-bit SHA-256 of (language, path, name, qualified_name, kind, signature)
  (`index/mod.rs`). It is stable across reindex but **changes on move / rename /
  signature change** (path is an input).
- Durable, content-derived signals that survive a move: the **bare symbol name**,
  the symbol **`kind`**, the **`signature`**, and the memory's
  **`source_text_hash`** (the bound chunk's text hash, captured at create time).

Conclusion: relocation must key off content-derived signals and must **rewrite the
volatile ids (including `binding_id`) it lands on**, so a relocated memory does not
re-enter the fallback on every subsequent validation.

## Goals / non-goals

Goals: cross-file symbol moves self-heal on reindex; a move is only auto-anchored
when content-confirmed (never a silent wrong bind); a first-class rebind operation
exists; non-current active memories are surfaced, never silently rotting.

Non-goals: fuzzy / best-effort name matching (rejected — silent wrong anchors
violate the provenance ethos); a distinct `ambiguous` anchor status (dropped —
non-relocatable bindings stay `gone`, which existing consumers already route
safely; the doctor recomputes candidates live); auto-relocation of path bindings
across file splits (no reliable single target — handled by doctor + rebind).

## Phase 1 — Engine self-heal (content-confirmed relocation)

### 1a. New durable columns on `repo_memory_bindings` (additive migration)

Add `symbol_kind TEXT` and `signature_hash TEXT` (nullable) to
`repo_memory_bindings`. Populate at create and on every relocation from the
resolved symbol (`symbols.kind`, `hex_sha256(symbols.signature)`). They corroborate
relocation and power doctor candidate ranking. New migration `Vnext` on top of the
current schema (`index/schema/migrations.rs`), with invariant comment + test per
the schema rules. `anchor_status` remains a raw `TEXT` value (no new value added,
so no enum/migration needed there — wording in the old draft that referenced
`as_db_str`/`from_db_str` for it was wrong; it is raw strings end-to-end).

### 1b. Bare-name relocation fallback

In `validate_symbol_binding` / `validate_logical_symbol_binding`, after the
existing exact-id and `qualified_name` lookups fail, before returning `gone`:

1. Derive `short_name` by stripping the persisted `"{binding.path}::"` prefix from
   `binding_id` (NOT "after the last `::`", which breaks for `impl`/C++ names that
   themselves contain `::`, e.g. `crate::Foo`).
2. Find candidates: `symbols` (or `logical_symbols` members) `WHERE name =
   short_name`, **joined through the unqualified `files` table so the query is
   context-scoped** the way the existing fallback is — otherwise rows from dead
   worktrees/other contexts appear as phantom candidates or get relocated onto a
   row the next `gc()` deletes.
3. Decide:
   - Exactly **one** candidate whose bound chunk `text_hash == source_text_hash`
     (memory's stored hash) **and** whose `kind`/`signature_hash` do not
     contradict the binding's stored values → **`relocated`**.
   - **Zero** content-hash matches, or **two or more** → **`gone`** (do not guess;
     doctor + rebind resolve it). `kind`/`signature_hash` never trigger a silent
     relocate on their own — a content-hash match is required.
4. On `relocated`, rewrite **all** volatile fields: `binding_id` (to the new
   qualified_name), `symbol_id`, `logical_symbol_id`, `path`, `chunk_id`,
   `start_line`, `end_line`, `symbol_kind`, `signature_hash`. The persist UPDATE in
   `memory/api.rs::validate_memories` keys on the old PK
   (`memory_id, binding_kind, binding_id`); it must SET the new `binding_id` while
   matching the old, and tolerate a PK conflict if a sibling binding already holds
   the new id (skip/merge, don't crash).

### 1c. Chunk bindings

`validate_chunk_binding` gains the same content-exact fallback: when the stored
`chunk_id` is gone, find a context-scoped chunk `WHERE text_hash =
source_text_hash`; unique → `relocated` (rewrite ids), else `gone`. (Chunk
bindings are the most fragile — chunk ids churn on any edit to the file.)

### 1d. Consumer audit

Because we are NOT adding a new status, the existing `anchor_status` consumers stay
correct, but the relocation changes must be verified against all of them:
`resolve.rs::split_active_stale` (routes `stale|gone|unverified` to the stale
bucket), the `RepoMemoryValidationReport` counts and `_ => unverified` catch-all
(`memory/api.rs::validate_memories`), and the `schema_bootstrap_tests.rs`
assertions. Wrap the validation pass in a single transaction (it now does repo-wide
name scans per gone binding + per-row writes).

## Phase 2 — `memory_rebind`

Re-anchor an existing memory without the recreate+obsolete dance (avoids the
`duplicate` trap).

- Core: `memory/api.rs::rebind_memory(conn, memory_id, bind: RepoMemoryBindTarget)`
  — replace the memory's binding row(s) with the new target, **refresh
  `repo_memories.source_text_hash` from the newly resolved binding** (else the next
  `validate_bound_chunk` immediately flips it to `stale`), replace
  `repo_memory_call_paths` rows when the prior binding was `call_path`, re-run
  `validate_binding`, return the resulting binding + status.
- MCP tool `memory_rebind { memory_id, bind: MemoryBindArgs }` (reuses
  `memory_create`'s bind shape) in `rag-rat-mcp`.
- CLI: `rag-rat memory rebind <memory_id> --symbol <name> | --path <p> | --chunk <id>`.
- `memory_create` duplicate detection is unchanged (correct as-is).

## Phase 3 — Surfacing

- `rag-rat memory doctor` — list active memories whose anchor is `gone | stale`;
  per memory recompute live candidates (context-scoped bare-name search, ranked by
  `kind` + `signature_hash` + name agreement) and print the suggested
  `memory rebind` command. For a `gone` memory with zero candidates, name the
  remediation explicitly (the code was deleted → `memory mark-obsolete`, vs moved →
  `memory rebind`). Exit non-zero when any `gone` remains (CI / hook gate), so the
  gate is actionable, not permanently red on legitimately-deleted code.
- Post-`index` / post-`reconcile` one-line notice when active memories are
  non-current: `⚠ N repo memories need re-anchoring — run 'rag-rat memory doctor'`.
- `index_status` (MCP) gains an anchor-health breakdown over active memories:
  `{ current, relocated, stale, gone }`.

## Testing

`crates/rag-rat-core/src/index/schema_bootstrap_tests.rs` (`repo_memory_*`) +
`memory/validate.rs` unit tests:

- cross-file move, identical body → `relocated`; then **edit the new file and
  reindex again** → still `current`/resolves via the rewritten `binding_id` (guards
  the durability regression, P1 #1).
- cross-file move, body edited (hash differs) → `gone` (not silently relocated).
- two same-named symbols, the bound one moves, body identical → unique hash match
  → `relocated`; two candidates both hash-match → `gone`.
- `impl` / C++ symbol whose `name` contains `::` → `short_name` derives correctly.
- candidate in another worktree context is not matched (context scoping).
- chunk-binding content-exact relocation.
- `rebind_memory` takes a `gone` memory to `current` and refreshes
  `source_text_hash` (no stale flap on the next validate).
- doctor exit codes (0 clean / non-zero when `gone` present).
- existing in-file relocation test stays green.

## Rollout

Phase 1 is the high-leverage core and lands first (self-heals on the next
reindex). Phases 2–3 build on it. Each phase: `cargo build`, `cargo clippy
--all-targets`, `cargo test -p rag-rat-core` green; `cargo +nightly fmt`.

## Deferred / noted (not in this spec)

- Closed `AnchorStatus` enum per the repo persisted-enum style rule (orthogonal
  cleanup; no new value is introduced here).
- Multi-chunk symbols: `chunk_for_symbol`/`chunk_for_logical_symbol` are `LIMIT 1`,
  so `source_text_hash` and hash-disambiguation see only the first chunk
  (pre-existing).
- Overloads sharing one `qualified_name` in a single file: the existing
  `qualified_name = ?` fallback already `LIMIT 1`-picks arbitrarily (pre-existing
  latent mis-bind).
