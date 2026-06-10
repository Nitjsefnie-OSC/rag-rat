# The Hypertext Repo-Memory Engine — Spec A: Linked Core

Status: design (approved in brainstorm 2026-06-09)
Scope: Spec A of a sequenced rollout. Spec B (memory embeddings + `memory_find_related`)
and Spec C (external links + advisory preflight) are summarized in the Roadmap and will
get their own design docs.

## Context

rag-rat's repo memories are typed, source-anchored notes (13 kinds, 5-state anchor model)
bound to code/git/GitHub evidence. Today each memory is an **island**: there is no
memory-to-memory relationship, no edit history, and the write API binds a memory to exactly
**one** anchor even though the storage already supports many. As a knowledge base grows, the
missing pieces are (1) the ability for one note to cover several code sites, (2) explicit
relationships between notes ("this decision supersedes that one"; "this risk is mitigated by
that invariant"), and (3) an audit trail of how a note changed over time.

Spec A delivers exactly those three, plus traversal and hydration to read them — **without any
new dependency or posture change**. It deliberately excludes the original proposal's two
invariant-colliding pieces: rag-rat will not gain an HTTP client and will not become an
edit-enforcement gatekeeper (see Roadmap / Rejected for why and what replaces them).

### Grounding in current code (verified)

- `repo_memory_bindings` is already 1-memory→many-bindings: PK `(memory_id, binding_kind,
  binding_id)`, and `RepoMemory.bindings` is a `Vec` (`crates/rag-rat-core/src/query/memory.rs:22`).
  The single-binding limit is purely in the write API: `RepoMemoryCreate.bind` is singular
  (`memory.rs:73`) and `create_memory` calls `resolve_binding` once → `insert_binding` once
  (`memory.rs:161,192`).
- Memory IDs are already `TEXT` (`mem_{ts_hex}_{input_hash[:12]}`), so link/history FKs drop in.
- A hop-limited BFS with dedup already exists for the code graph:
  `query/graph.rs::traverse_with_options` (`:270`) + `dedupe_hops` (`:414`) — the shape to mirror
  for link traversal.
- `validate_kind()` (`memory.rs:~1529`) is the pattern for validating a fixed vocabulary.
- `next_tools` suggestion objects: `RepoBriefNextTool { tool, reason, args }`
  (`query/repo_brief.rs:126`) — the surface for *suggesting* links, never auto-creating them.
- MCP tool registration: `#[tool_router]` in `crates/rag-rat-mcp/src/server.rs:41`; static
  `TOOL_NAMES` list (`tools.rs:124`); per-tool `*Args` structs deriving `schemars::JsonSchema`.
- Schema migrations run to V013 (`index/schema.rs`); next free ids are V014, V015.

## Goals

1. One memory can bind to multiple code/evidence anchors (create-time and incrementally).
2. Typed, directed memory-to-memory links with declared inverses; traversable as a subgraph.
3. An append-only edit history for memories, stamped with the git commit context.
4. High-context reads: hydrate linked-memory bodies inline to a caller-controlled depth.

## Non-goals (Spec A)

- No outbound network calls (no provenance fetching) — deferred to Spec C as advisory-only.
- No edit interception / enforcement — architecturally impossible for an MCP server; deferred
  to Spec C as advisory surfacing only.
- No memory embeddings / semantic `memory_find_related` — that's Spec B.
- No authoritative numeric "risk score". `graph_context` counts are descriptive and carry a
  `scoring_note`, matching `repo_brief`'s posture.

## Design

### 1. Multi-binding

- `RepoMemoryCreate.bind: RepoMemoryBindTarget` → `binds: Vec<RepoMemoryBindTarget>`
  (validate non-empty). `create_memory` loops `resolve_binding` + `insert_binding` over each;
  `source_text_hash` is taken from the first resolvable code anchor (unchanged semantics for the
  single-bind case).
- New tools **`memory_bind`** (add one anchor to an existing memory) and **`memory_unbind`**
  (remove one by `(binding_kind, binding_id)`). Both write only `repo_memory_bindings`.
- **Dedup becomes additive.** Today `duplicate_memory_id` keys on `(title, body, binding)`.
  Change it to key on content `input_hash` only (kind+title+body+tags). On a content match,
  do not reject — **merge the incoming binds** into the existing memory and return it with
  `duplicate: true`. "The same invariant also applies here" is now a bind, not a rejection.
- MCP arg change: `MemoryCreateArgs.bind` → `binds: Vec<MemoryBindArgs>` (`tools.rs:~450`).

### 2. Hypertext links — migration V014

```sql
CREATE TABLE repo_memory_links (
    from_memory_id TEXT NOT NULL REFERENCES repo_memories(id),
    to_memory_id   TEXT NOT NULL REFERENCES repo_memories(id),
    link_kind      TEXT NOT NULL,
    created_by     TEXT,
    created_at_ms  INTEGER NOT NULL,
    PRIMARY KEY (from_memory_id, to_memory_id, link_kind)
) STRICT;
CREATE INDEX idx_repo_memory_links_to ON repo_memory_links(to_memory_id);
```

- **No `ON DELETE CASCADE`** — consistent with obsolete-not-delete. `memory_unlink` removes a row
  explicitly; marking a memory obsolete leaves its links intact (and visibly pointing at an
  obsolete node).
- `link_kind` is a **closed `LinkKind` enum** (per the `rust-modern-style` skill's persisted-enum
  rule — *not* a raw string), with a stable `as_db_str()` / `from_db_str()` and an `inverse()`
  method. Vocab with inverses: `justified_by⟷justifies`, `supersedes⟷superseded_by`,
  `mitigates⟷mitigated_by`, `refines⟷refined_by`, `depends_on⟷required_by`, self-inverse
  `contradicts`, `related`. Store one directed row; derive backlinks via `inverse()`. A round-trip
  test asserts every variant survives `as_db_str` → `from_db_str`. (This intentionally diverges
  from rag-rat's existing string-validated memory `kind`; the inverse logic wants an enum anyway.)
- Tools: **`memory_link`** `{from_memory_id, to_memory_id, link_kind}`, **`memory_unlink`**.

### 3. Edit history — migration V014 (same migration)

```sql
CREATE TABLE repo_memory_history (
    history_id        TEXT PRIMARY KEY,
    memory_id         TEXT NOT NULL REFERENCES repo_memories(id),
    kind TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL,
    confidence TEXT NOT NULL, status TEXT NOT NULL,
    updated_by        TEXT,
    commit_context_sha TEXT,
    archived_at_ms    INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_repo_memory_history_lookup ON repo_memory_history(memory_id);
```

- `update_memory` (`memory.rs:200`) snapshots the **pre-update** row into history before writing,
  stamping `commit_context_sha` with current `HEAD` (rag-rat already resolves HEAD for indexing).
  `mark_obsolete` flows through `update_memory`, so obsoleting is captured too.
- Optional read tool `memory_history(memory_id)` returns the snapshots newest-first.

### 4. Traversal + hydration

- **`memory_graph_traverse`** `{ start_memory_id, hop_limit=3, allowed_link_kinds?,
  inline_body_depth=0 }`. BFS over `repo_memory_links` mirroring
  `graph.rs::traverse_with_options` semantics + dedup. Returns the node plus `links.outward`
  and `links.backlinks` (each `{to_id|from_id, kind, title}`), with `body_inlined` populated for
  hops within `inline_body_depth`. Includes a `graph_context`
  `{ connected_component_size, stale_dependency_count }` + `scoring_note`.
- Reading a single memory (`memory_by_id` path / a `memory_get` tool) hydrates inline link bodies
  to `inline_body_depth`, extending `attach_memory_children`.

### 5. Link suggestion (no auto-create)

- When listing/reading memories, surface **suggested** links via `next_tools` entries
  (`memory_link` with a `reason`), derived from shared bindings (memories anchored to overlapping
  symbols/paths/edges). Suggestions are advisory; the agent runs `memory_link` to confirm.
  (Semantic-similarity-driven suggestions arrive with Spec B.)

## Affected files

- `crates/rag-rat-core/src/index/schema.rs` — V014 (links + history).
- `crates/rag-rat-core/src/query/memory_links.rs` (new) — `LinkKind` enum
  (`as_db_str`/`from_db_str`/`inverse`), link CRUD, traversal. Keeps `memory.rs` focused.
- `crates/rag-rat-core/src/query/memory.rs` — multi-bind create, additive dedup, `memory_bind`/
  `memory_unbind`, history snapshot on update, inline-body hydration.
- `crates/rag-rat-mcp/src/tools.rs` + `server.rs` — register `memory_bind`, `memory_unbind`,
  `memory_link`, `memory_unlink`, `memory_graph_traverse`, `memory_history`; update
  `MemoryCreateArgs` to `binds`.
- `crates/rag-rat-core/src/index/mod.rs` — public `Database` methods for the new core fns.
- `README.md` + `docs/mcp-tools.md` — document the new tools and the link vocabulary.

## Verification

1. `cargo test -p rag-rat-core` — unit tests for: multi-bind create, additive-dedup merge,
   bind/unbind, link/unlink + inverse backlink derivation, `LinkKind` `as_db_str`→`from_db_str`
   round-trip (every variant) + invalid-string rejection,
   history snapshot capture (one row per update, correct `commit_context_sha`), and
   hop-limited traverse with `inline_body_depth` (depth 0 vs N).
2. `cargo test -p rag-rat-core` migration test: V014 applies on a V013 DB; idempotent re-run.
3. `rag-rat migrate --check` then `rag-rat migrate` on the self-index DB.
4. Dogfood via MCP against rag-rat's own index: `memory_create` with two binds → `memory_link`
   two notes → `memory_graph_traverse` and confirm outward/backlinks + inlined bodies; edit one
   and confirm `memory_history` shows the prior version.
5. `rag-rat eval` — current-source violations must stay at zero.
6. Size check per `docs/binary-size.md` (expected: no change — no new deps in Spec A).

## Roadmap (own specs)

- **Spec B — Semantic relatedness.** Embed memory `title+body` via the existing FastEmbed
  pipeline (new vector storage parallel to `chunk_embeddings`, reconcile policy entry,
  migration V015). `memory_find_related` = vector cosine (reuse `search/lexical.rs::dot()`) ∪ FTS
  ∪ graph expansion from Spec A.
- **Spec C — External links + advisory preflight.** `repo_memory_external_links` table (URL,
  title, category, optional agent-recorded `content_hash_baseline`) as **local metadata only**.
  `memory_preflight_for_path` + `impact_surface` enrichment surface bound Invariants/specs and,
  via `next_tools`, tell the **agent** to fetch the URL with its own tools. rag-rat never makes
  network calls and never enforces edits.

## Rejected (from the original proposal)

- **rag-rat fetching external URLs** (404 / drift checks). Collides with the read-only/local
  contract, adds a heavyweight HTTP+TLS dependency against the size budget, and is an SSRF
  surface. Replaced by Spec C's advisory model: the agent fetches; rag-rat only stores/surfaces.
- **Edit-pipeline gatekeeper (`BlockAndPrompt`).** An MCP server is request/response and cannot
  intercept the agent's `Write`/`Edit`. Enforcement belongs to the harness (e.g. `PreToolUse`
  hooks). rag-rat surfaces guardrail evidence; it does not block.
- **`DATETIME`/`CURRENT_TIMESTAMP`, `ON DELETE CASCADE`.** Replaced with `_at_ms INTEGER` and
  no-cascade to match house schema conventions and the obsolete-not-delete invariant.
