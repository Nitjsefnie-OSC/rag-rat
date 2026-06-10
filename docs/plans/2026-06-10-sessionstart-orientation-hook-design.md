# Design — SessionStart orientation hook (+ directory memories + indexed tree)

Status: design in progress, **revised after Fable 5 spec review, Claude Code docs
verification, and an unbiased Fable 5 orientation experiment in the held monorepo.**
Uncommitted (repo convention).

## Problem

The PreToolUse grep-augmentation hook pushes *local* context but never gives a
whole-repo orientation, and nothing tells an agent that a rag-rat knowledge-base
layer exists to query. We want, at session start (and after `/clear` and
compaction), to inject a tight orientation that (a) **announces the rag-rat MCP
layer and nudges its use**, and (b) **orients** — what the repo is, where things
are, the core sections — computed read-only, never blocking.

An unbiased agent dropped into the held monorepo (exploring only via the rag-rat
CLI) reported what actually helped it orient, and what was noise. Findings drive
this design:
- **Most useful:** a one-line *purpose*; *what each top directory is*; the
  **load-bearing files** (spine, especially `fan_in` — "`database.rs fan_in=2286`
  told me everything goes through this"); a subsystem **table of contents**;
  **recent activity**; and the **actual text of active memories** (not counts).
- **Noise at startup:** raw metrics (chunk counts, churn_per_kloc, scores), the
  refactor lenses (god_modules/refactor_candidates/churn), `split_hints`,
  `next_tools`, generated files listed as hotspots, index plumbing, and memory
  *counts*. ("I used exactly two numbers all session: fan_in and file count.")
- **Key gap:** *what each directory means* is the one thing metrics can't supply —
  and there's no CLI way to read a memory's text.

Resolution: structure comes from an **indexed tree builder**; per-directory meaning
comes from **directory-scoped memory titles** on the tree (authored as repo
memories — the hardened AGENTS.md already pushes agents to write these, so the tree
self-enriches over time); the repo purpose is a **root-scoped memory**.

## Claude Code facts (verified against https://code.claude.com/docs/en/hooks)

- `SessionStart` sources: `startup | resume | clear | compact` (stdin `"source"`).
- **Matchers accept `|`-alternation** of exact strings → `"startup|clear|compact"`
  is valid. We also self-filter `source` in-hook (so the feature can't silently die
  if matcher semantics ever differ).
- **Plain stdout (exit 0) is injected into context** for SessionStart (unlike
  PreToolUse where it's debug-only). We use plain stdout.
- Synchronous before first prompt; default command timeout 600s → we set an explicit
  short `timeout` (5s). Only `type:"command"`/`"mcp_tool"` supported.
- **MUST VERIFY at impl:** whether SessionStart fires for Task/subagent spawns (if
  so, gate so the digest doesn't inject into every subagent). Silent posture bounds
  the blast radius regardless.

## Decisions

- **Content (from the unbiased run):** purpose + annotated dir tree + load-bearing
  files (fan_in) + recent activity + concise tool-nudge + warnings-only watcher-aware
  health + active-memory text (capped). **No raw metrics, no refactor lenses.**
  ~30–40 lines is acceptable (the orienting agent asked for this richness — the
  enemy is metrics, not lines).
- **Triggers:** `startup`, `clear`, `compact` (not `resume`).
- **One binary:** reuse `rag-rat claude-hook`, branch on `hook_event_name`.
- **Read-only + fast:** `IndexConnection::open_read_only`; no reindex; never block.
- **Output:** plain stdout, exit 0, silent on any error / non-rag-rat repo.

---

## Phase A — Directory-scoped memories (+ CLI read)

Memories can already bind to symbol/logical/chunk/edge/path/commit/github. Add a
**directory** anchor so a memory can describe a whole subsystem (and the repo root).

- **Binding kind `"dir"`**, `binding_id` = normalized directory path (no trailing
  slash; repo root = `""`). New optional `dir: Option<String>` on
  `RepoMemoryBindTarget` / MCP `MemoryBindArgs`; `resolve_binding` routes it to
  `resolve_dir_binding` (`query/memory/resolve.rs`).
- **Validation** (`query/memory/validate.rs::validate_dir_binding`): a dir binding is
  `current` iff at least one indexed file is under it —
  `EXISTS(SELECT 1 FROM files WHERE path = ?dir OR path LIKE ?dir || '/%')`
  (root `""` → any file exists); else `gone`. No relocation (directories don't move
  like symbols; gone-if-empty is correct). `source_text_hash` is unused (dir
  bindings are descriptive, not content-anchored), so they never go `stale`.
- **Schema:** none. `binding_kind`/`binding_id` are already `TEXT`; `"dir"` is a new
  value. (Add it to the `validate_binding` match + any `anchor_status` consumers'
  awareness — there are none that special-case kind beyond the dispatch.)
- **CLI read surface** (the unbiased gap): add `rag-rat memory list [--kind dir]`
  and `rag-rat memory show <id>` (read-only, print title+body+binding) so dir
  memories — and all memories — are inspectable from the CLI. (Authoring dir
  memories is via MCP `memory_create {bind:{dir:…}}`, the path agents already use.)
- **Tests:** dir memory over an indexed dir → `current`; dir with no files → `gone`;
  root memory (`dir:""`) → `current`; `memory show`/`list` render dir memories.

## Phase B — Indexed annotated tree builder

New core module `query/tree.rs`: `pub fn dir_tree(conn: &Connection, opts: &TreeOpts)
-> DirTree`, built read-only from the **scoped** `files` (same context-scoping view
as Phase-C, see below). Produces a compact, "not too deeply nested" map.

Pruning / shape (confirmed):
- Walk directories from repo root; **collapse single-child chains** (render
  `a/b/c/` as one node when intermediate dirs have no memory and no direct files).
- Include a directory as its own node iff: it has a **directory memory**, OR it
  **directly contains ≥ N source files** (N≈3, tunable), OR it's a necessary parent
  of an included node.
- **Cap depth ≤ 3** from root and total nodes ≤ ~25 (size guard); siblings beyond
  the cap fold into a `… (+k more)` tail.
- **Skip** hidden (`.*`), `target/`, and other config-excluded dirs. (No special
  "generated" rule — a `packages/held-core/ ‹GENERATED — don't edit›` dir *memory*
  conveys that, self-documenting.)
- **Annotate** each node with its direct source-file count and, if a `"dir"` memory
  binds exactly that path, its **title**. The **root** dir memory's title is surfaced
  separately as the digest's purpose line.

`DirTree` is owned/flat: `Vec<TreeNode { depth: u8, label: String, file_count: u32,
memory_title: Option<String> }>` + `root_memory_title: Option<String>`.

Independently useful beyond the hook (e.g. a `rag-rat tree` CLI command — optional,
not required by this spec).

## Phase C — SessionStart orientation hook

### C1. Dispatch — `crates/rag-rat-cli/src/claude_hook.rs`

`HookInput` corrected: add `hook_event_name: Option<String>` + `source:
Option<String>`; demote `tool_name`/`tool_input` to `#[serde(default)]` (SessionStart
stdin lacks them → without this, serde fails → permanent silent no-op). `run_inner`
branches on `hook_event_name`:
- `Some("SessionStart")` → SessionStart path; otherwise → existing grep path.

SessionStart path: in-hook allowlist (`source ∈ {startup,clear,compact}`, else
silent) → `find_config` (None ⇒ silent) → if the DB file is absent, print the
attribution header + `index not built — run 'rag-rat index'` (do **not** open/create
the DB) → else `IndexConnection::open_read_only`, compose the digest, print to
**stdout**, exit 0. Any error on the path ⇒ print nothing, exit 0. **No stray
`println!` on this branch** — its stdout is model context.

### C2. Read-only data — core `query/orientation.rs`

`pub fn orientation(conn, root, opts) -> Orientation` — pure index read. It:
- **Installs the context-scoping temp view** on the read-only connection (extract
  the view DDL from `index/lifecycle.rs::set_context` into a shared
  `install_scope_view(conn, commit_sha, worktree_id)` called by both `set_context`
  and here; active context from `resolve_git_context(root)`; temp DDL works on
  `mode=ro`) so all `files`-based reads are scoped to the active worktree, not the
  duplicate `main.files` rows of other contexts (Fable P2.1).
- `dir_tree(conn, …)` (Phase B) → annotated layout + `root_memory_title`.
- **load-bearing files:** `repo_brief(conn, Spine, top_n=5)` → keep only `path` +
  `fan_in` (drop all other metrics).
- **recent activity:** last ~5 commit subjects + the few hottest recently-changed
  source files, from the indexed git history (read-only).
- **active memories (capped):** titles of active memories **not** already shown as
  dir-tree nodes, capped at ~5 (`+k more` tail); dir-memory titles live in the tree.
- `git_history::status(conn, root)` → `head`/`indexed_head` (equality only).
- anchor-health counts (`memory::anchor_health_counts`, same crate).

`Orientation` is owned/flat. Watcher state is computed in the CLI hook (C3) and
combined at format time.

### C3. Formatting + watcher-aware health — CLI

Plain-text digest, led by attribution + capability nudge, then orientation. Example
(held repo, illustrative):

```
▶ rag-rat repo intelligence — injected by the rag-rat MCP server (prefer it over grep/cat)
  concept → semantic_search · callers/callees → find_callers/trace_callees
  before editing a symbol → impact_surface · exact symbol → symbol_lookup
  why/rationale → repo memories ride along; memory_search to dig

nooklet (codename "held") — local-first, on-device-AI RN+Rust app for parents of ND kids
LAYOUT  (‹…› = directory memory)
  core/held-core/src/   ‹single Rust crate — all business logic›
    actors/  ‹per-domain actors: msg/handle/actor›   data/ ‹SQLCipher›   runtime/ topology/ search/
  apps/mobile/  ‹Expo RN — UI, thin over FFI›   apps/web/  apps/wear*/
  packages/held-core/  ‹GENERATED ubrn bindings — don't edit›   docs/
load-bearing: data/database.rs (fan_in 2286) · iroh/actor.rs · search/adapter.rs · nav/Root.tsx
recent: <5 commit subjects> · hot: JournalScreen.tsx, data/tests.rs
memories: <≤5 active titles not on the tree> [+k more — memory_search]
health: index fresh (watcher live)
```

The purpose line is the **root dir memory's title** (omitted if none — its absence
is itself a nudge to write one). LAYOUT is the Phase-B tree. `load-bearing` shows
only path + `fan_in`. Health is **watcher-aware**:
- watcher live (election lock held), fresh → `index fresh (watcher live)`.
- watcher live, behind → `index syncing (watcher live)` — transient, no nudge.
- watcher enabled, not running, behind → `index stale — start the rag-rat MCP server`.
- watcher disabled (`RAG_RAT_NO_WATCH`/`watch.enabled=false`), behind →
  `watcher off; index stale — run 'rag-rat index'`.

Watcher liveness = non-blocking `try_lock` on the per-worktree election lock
(`locks.rs`); combined with `config.watch.enabled`. `memory doctor` nudge only when
`gone > 0`; parser-failure note only when `> 0`. Never claims commit distance.

### C4. Settings management — `crates/rag-rat-cli/src/claude_settings.rs`

Generalize to a second event with **per-event presence semantics** (Fable P2.6):
PreToolUse keeps per-matcher entries (`Grep`,`Bash`); **SessionStart** is one
is-ours entry — matcher `"startup|clear|compact"`, command `rag-rat claude-hook`,
`timeout: 5` — detected by `is_ours` (not exact matcher string) and **replaced** if
matcher/timeout differ (so a future change can't leave a duplicate firing the digest
twice). `install`/`uninstall`/`status` cover both; prune empty containers; preserve
foreign entries; refactor the `(bool,bool)` status return into a named struct.
`--global` unchanged (status warns if both global+project install ours → double
inject).

### C5. CLI wiring

`rag-rat hooks install|uninstall|status --claude [--global]` covers both events via
the generalized helpers. Update `hooks status` + `usage()`.

## Testing

- **A:** dir/root binding resolution (current/gone); `memory show`/`list`.
- **B:** tree from a fixture: depth cap, single-child collapse, ≥N-file inclusion,
  dir-memory title annotation, root-memory purpose, scoped (duplicate-path fixture →
  no inflation, Fable P2.1).
- **C:** SessionStart JSON lacking `tool_name` deserializes (P2.3); source allowlist
  (resume → silent); non-rag-rat cwd → silent; DB absent → one-liner, **no DB
  created** (P1.1/P2.5); watcher-aware freshness lines selected correctly; settings
  install both events, matcher-drift replaces not duplicates (P2.6), uninstall prunes,
  foreign entries preserved, garbage settings don't crash; digest composed via
  `open_read_only` performs no writes / no graph rebuild.

## Rollout

Sequence **A → B → C** (A and B are independently useful and unblock C). Each phase:
`cargo build/clippy --workspace --all-targets` clean, `cargo test --workspace`
(`sigusr1_*` pre-existing/flaky), `cargo +nightly fmt --all`. Manual: author a few
dir memories (incl. root), install the hook, confirm the digest on a fresh
session/`/clear`/compaction; `--resume` doesn't fire.

## Open items to verify at implementation start

1. SessionStart firing on Task/subagent spawns (gate if so).
2. `cwd` present on SessionStart stdin (assumed by `find_config`).
3. Tree pruning constants (N files, depth, node cap) — tune against the held repo so
   the layout reads as a clean ToC, not a dump.
