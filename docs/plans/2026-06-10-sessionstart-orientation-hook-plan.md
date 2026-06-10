# SessionStart Orientation Hook — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use `- [ ]`.
>
> Spec: `docs/plans/2026-06-10-sessionstart-orientation-hook-design.md`. This plan + spec stay **uncommitted** (repo convention). Per-task `git commit` steps commit implementation code; **commit directly to `main`** (repo preference); no Co-Authored-By trailer.

**Goal:** A `SessionStart` Claude Code hook that injects a read-only repo orientation (purpose + indexed directory tree annotated with directory-memory titles + load-bearing files + recent activity + tool nudge + watcher-aware health), built on two reusable additions: directory-scoped memories and an indexed tree builder.

**Architecture:** Phase A adds a `"dir"` memory binding kind (anchor a memory to a directory / repo root). Phase B adds a read-only indexed tree builder annotated with those dir-memory titles. Phase C adds the hook: `rag-rat claude-hook` branches on `hook_event_name`, composes the digest read-only via `open_read_only` + a shared context-scoping view, prints plain stdout, exit 0 always; `rag-rat hooks install` registers a `SessionStart` entry.

**Tech Stack:** Rust 2024, rusqlite, serde, sha2. Crates: `rag-rat-core`, `rag-rat-mcp`, `rag-rat-cli`. Fixture harness: `unique_temp_root`/`source_config`/`IndexDatabase::rebuild` in `crates/rag-rat-core/src/index/schema_bootstrap_tests.rs`.

**Per-task gate:** `cargo test -p rag-rat-core <name>`; phase end: `cargo clippy --workspace --all-targets` clean, `cargo test --workspace` (ignore pre-existing/flaky `sigusr1_*`), `cargo +nightly fmt --all`.

---

## File structure

| File | Change |
|---|---|
| `crates/rag-rat-core/src/query/memory/mod.rs` | `dir` field on `RepoMemoryBindTarget` + `ResolvedBinding` |
| `crates/rag-rat-core/src/query/memory/resolve.rs` | `resolve_dir_binding`, route in `resolve_binding`, normalize dir |
| `crates/rag-rat-core/src/query/memory/validate.rs` | `validate_dir_binding` + `"dir"` dispatch arm |
| `crates/rag-rat-core/src/query/memory/api.rs` | `list_memories` / `memory_by_id` read helpers (for CLI) |
| `crates/rag-rat-mcp/src/tools/mod.rs` | `dir` on `MemoryBindArgs` + `From` impl |
| `crates/rag-rat-cli/src/commands.rs` | `memory list` / `memory show`; later digest plumbing |
| `crates/rag-rat-core/src/index/lifecycle.rs` | extract `install_scope_view` (shared) |
| `crates/rag-rat-core/src/index/scope.rs` (new) | `install_scope_view` home (or keep in lifecycle) |
| `crates/rag-rat-core/src/query/tree.rs` (new) | `dir_tree` + `DirTree`/`TreeNode`/`TreeOpts` |
| `crates/rag-rat-core/src/query/orientation.rs` (new) | `orientation()` read-only composer |
| `crates/rag-rat-cli/src/claude_hook.rs` | dispatch on event; SessionStart path; digest formatting |
| `crates/rag-rat-cli/src/claude_settings.rs` | generalize to PreToolUse + SessionStart |
| `crates/rag-rat-core/src/index/schema_bootstrap_tests.rs` | tests for A/B/C |

---

# Phase A — Directory-scoped memories

### Task A1: `dir` bind target + resolver

**Files:** `query/memory/mod.rs`, `query/memory/resolve.rs`

- [ ] **Step 1 — failing test** (`schema_bootstrap_tests.rs`): build a temp index with files under `src/`, create a memory bound to `dir:"src"`, assert it reads back with one binding `binding_kind=="dir"`, `binding_id=="src"`, `anchor_status=="current"`.

```rust
#[test]
fn dir_memory_binds_to_a_directory() {
    // unique_temp_root + source_config + rebuild; create a file under src/
    // db.memory_create(RepoMemoryCreate{ kind:"Decision", title, body, confidence:"high",
    //   bind: RepoMemoryBindTarget{ dir: Some("src".into()), ..default }, .. })
    // assert binding.binding_kind == "dir" && binding.anchor_status == "current"
}
```

- [ ] **Step 2 — run, expect FAIL** (`dir` field doesn't exist): `cargo test -p rag-rat-core dir_memory_binds`.

- [ ] **Step 3 — add the field** to `RepoMemoryBindTarget` (mod.rs, after `path`): `pub dir: Option<String>,`. `ResolvedBinding` already carries `binding_kind`/`binding_id`/`path`; no new field needed there (dir reuses them).

- [ ] **Step 4 — route + resolve** in `resolve.rs`. In `resolve_binding`, before the `path` arm:

```rust
    if let Some(dir) = bind.dir.as_deref() {
        return resolve_dir_binding(conn, dir);
    }
```

Add:

```rust
/// Normalize a directory anchor: trim, drop leading "./", strip a trailing "/".
/// Repo root is the empty string.
pub(crate) fn normalize_dir(dir: &str) -> String {
    let d = dir.trim().trim_start_matches("./").trim_end_matches('/');
    d.to_string()
}

pub(crate) fn resolve_dir_binding(conn: &Connection, dir: &str) -> anyhow::Result<ResolvedBinding> {
    let dir = normalize_dir(dir);
    // current iff at least one indexed file is under this dir (root "" => any file)
    let exists = dir_has_files(conn, &dir)?;
    Ok(ResolvedBinding {
        binding_kind: "dir".to_string(),
        binding_id: dir.clone(),
        path: Some(dir),
        start_line: None, end_line: None,
        logical_symbol_id: None, symbol_id: None, chunk_id: None, edge_id: None,
        commit_hash: None, github_owner: None, github_repo: None, github_number: None,
        call_path: None, source_text_hash: None,
        symbol_kind: None, signature_hash: None,
        anchor_status: if exists { "current" } else { "gone" }.to_string(),
    })
}

/// Read: are there indexed files at or under `dir`? Root ("") => any file exists.
pub(crate) fn dir_has_files(conn: &Connection, dir: &str) -> anyhow::Result<bool> {
    let n: i64 = if dir.is_empty() {
        conn.query_row("SELECT EXISTS(SELECT 1 FROM files)", [], |r| r.get(0))?
    } else {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM files WHERE path = ?1 OR path LIKE ?1 || '/%')",
            [dir], |r| r.get(0),
        )?
    };
    Ok(n != 0)
}
```

- [ ] **Step 5 — run, expect PASS**: `cargo test -p rag-rat-core dir_memory_binds`.
- [ ] **Step 6 — commit**: `git add crates/rag-rat-core/src/query/memory/ crates/rag-rat-core/src/index/schema_bootstrap_tests.rs && git commit -m "feat(memory): directory-scoped binding ('dir' kind)"`

### Task A2: validation for `dir` bindings

**Files:** `query/memory/validate.rs`

- [ ] **Step 1 — failing tests**: a `dir` memory over a populated dir validates `current`; over a path with no indexed files validates `gone`; root (`dir:""`) validates `current`.

```rust
#[test]
fn dir_memory_validation_current_and_gone() {
    // create dir memory on "src" -> memory_validate -> current==>=1, gone==0
    // create dir memory on "does/not/exist" -> reads gone on validate
}
```

- [ ] **Step 2 — run, expect FAIL** (no `"dir"` arm → falls to `_ => unverified`).

- [ ] **Step 3 — add the dispatch arm + validator** in `validate.rs`. In `validate_binding`'s match, add `"dir" => validate_dir_binding(conn, binding),`. Then:

```rust
pub(crate) fn validate_dir_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    // dir bindings are descriptive (no source_text_hash) -> current while files exist, else gone.
    let dir = binding.path.clone().unwrap_or_else(|| binding.binding_id.clone());
    Ok(if dir_has_files(conn, &dir)? { "current" } else { "gone" }.to_string())
}
```

- [ ] **Step 4 — run, expect PASS**. **Step 5 — commit**: `feat(memory): validate directory bindings`.

### Task A3: MCP bind surface for `dir`

**Files:** `crates/rag-rat-mcp/src/tools/mod.rs`

- [ ] **Step 1** — add `pub dir: Option<String>,` to `MemoryBindArgs` (mod.rs:434) and map it in `impl From<MemoryBindArgs> for RepoMemoryBindTarget` (mod.rs:612): `dir: value.dir,`. (Confirm the `RepoMemoryBindTarget` literal in that impl sets every field; add `dir`.)
- [ ] **Step 2** — `cargo build -p rag-rat-mcp` compiles; the existing `memory_create` tool now accepts `bind:{dir:"…"}`.
- [ ] **Step 3 — commit**: `feat(mcp): accept dir bind target in memory_create`.

### Task A4: CLI read surface — `memory list` / `memory show`

**Files:** `crates/rag-rat-core/src/query/memory/api.rs` (read helper + facade in `query_api.rs`), `crates/rag-rat-cli/src/commands.rs`

- [ ] **Step 1 — failing test**: `list_memories(conn, None)` returns created memories (id/kind/title/status/binding summary); `--kind dir` filters to dir bindings.

```rust
pub(crate) struct MemorySummary { pub memory_id: String, pub kind: String, pub title: String,
    pub status: String, pub binding_kind: String, pub binding_id: String }
pub(crate) fn list_memories(conn: &Connection, kind: Option<&str>) -> anyhow::Result<Vec<MemorySummary>>;
```

- [ ] **Step 2 — implement** `list_memories` (read-only join `repo_memories` + first binding; optional `WHERE binding_kind = ?`), and reuse existing `memory_by_id` for `show`. Add facades `IndexDatabase::list_memories` / `memory_get` mirroring the `memory_doctor` facade.
- [ ] **Step 3 — CLI**: in `commands.rs::memory` add arms `"list"` (print `id  [kind/status]  title  (binding)`; `--kind dir` filter) and `"show" <id>` (print title, body, bindings via `db.memory_get`). Update the `memory command needs a subcommand` + `unknown memory subcommand` help strings to include `list, show`.
- [ ] **Step 4** — run + manual `rag-rat memory list`/`show <id>`; **commit**: `feat(cli): rag-rat memory list/show`.

### Task A5: Phase-A gate
- [ ] `cargo clippy --workspace --all-targets` clean; `cargo test -p rag-rat-core memory` green; `cargo +nightly fmt --all`; commit any fmt.

---

# Phase B — Indexed annotated tree builder

### Task B1: extract `install_scope_view` (shared)

**Files:** `crates/rag-rat-core/src/index/lifecycle.rs` (+ optional `index/scope.rs`)

The context-scoping `temp.files` view + `temp.connection_context` table currently lives inline in `set_context` (lifecycle.rs). Extract it so a read-only connection can install the same scoping.

- [ ] **Step 1 — extract** a `pub(crate) fn install_scope_view(conn: &Connection, commit_sha: &str, worktree_id: &str) -> rusqlite::Result<()>` containing the exact `execute_batch` blocks now in `set_context` (the `connection_context` create+inserts and the `DROP VIEW temp.files; CREATE TEMP VIEW temp.files AS …` union). Have `set_context` call it. No behavior change.
- [ ] **Step 2** — `cargo test -p rag-rat-core` (existing context/scoping tests still pass). **Commit**: `refactor(index): extract install_scope_view from set_context`.

### Task B2: `dir_tree`

**Files:** `crates/rag-rat-core/src/query/tree.rs` (new), `crates/rag-rat-core/src/query/mod.rs` (add `pub mod tree;`)

- [ ] **Step 1 — failing test** (`schema_bootstrap_tests.rs`): index files under `src/a/x.rs`, `src/a/y.rs`, `src/b/z.rs`, a generated `gen/g.rs`; create a dir memory on `src/a` titled "alpha core". `dir_tree` returns nodes for `src/a` (file_count 2, memory_title Some("alpha core")) and `src/b`, collapses single-child chains, respects depth cap, and a duplicate-path fixture does NOT inflate counts (scoped).

- [ ] **Step 2 — implement** (read-only; caller installs the scope view first so `files` is scoped):

```rust
use rusqlite::Connection;

pub struct TreeOpts { pub max_depth: u8, pub min_files: u32, pub max_nodes: usize }
impl Default for TreeOpts { fn default() -> Self { Self { max_depth: 3, min_files: 3, max_nodes: 25 } } }

pub struct TreeNode { pub depth: u8, pub label: String, pub path: String,
    pub file_count: u32, pub memory_title: Option<String> }
pub struct DirTree { pub nodes: Vec<TreeNode>, pub root_memory_title: Option<String>, pub truncated: u32 }

pub fn dir_tree(conn: &Connection, opts: &TreeOpts) -> anyhow::Result<DirTree> {
    // 1. SELECT path FROM files (scoped view) ; derive per-directory direct-file counts.
    // 2. Build the dir set: include a dir if it has a "dir" memory OR direct file_count >= min_files;
    //    plus ancestors needed to reach included dirs (up to max_depth); collapse single-child chains
    //    (a dir with exactly one included child and no memory/direct files folds into the child label).
    // 3. Order by path; cap at max_nodes (record `truncated` = dropped count).
    // 4. Annotate: file_count (direct), and memory_title from a "dir" memory whose binding_id == path
    //    (active memories only). root_memory_title from the dir memory with binding_id == "".
    // Returns owned DirTree.
    todo!("implement per the steps above; see dir-memory query below")
}

/// active "dir" memory titles keyed by their directory path (binding_id).
fn dir_memory_titles(conn: &Connection) -> anyhow::Result<std::collections::HashMap<String, String>> {
    let mut stmt = conn.prepare(
        "SELECT b.binding_id, m.title FROM repo_memory_bindings b
         JOIN repo_memories m ON m.id = b.memory_id
         WHERE b.binding_kind = 'dir' AND m.status = 'active'")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?)))?;
    rows.collect::<Result<_,_>>().map_err(Into::into)
}
```

Replace the `todo!` with the real build (directory aggregation from the `path` column: split on `/`, count direct files per dir, apply inclusion/collapse/cap rules). Keep all SQL read-only.

- [ ] **Step 3 — run, expect PASS** (`cargo test -p rag-rat-core dir_tree`). Tune `min_files`/`max_depth` against `/home/kk/src/held` so the layout reads as a clean ToC (open item).
- [ ] **Step 4 — commit**: `feat(query): indexed dir_tree with directory-memory annotations`.

### Task B3: Phase-B gate
- [ ] clippy/test/fmt as Phase-A gate.

---

# Phase C — SessionStart orientation hook

### Task C1: `orientation()` read-only composer

**Files:** `crates/rag-rat-core/src/query/orientation.rs` (new), `query/mod.rs`

- [ ] **Step 1 — failing test**: over a built index (with a dir memory + a root memory), `orientation(conn, root, &OrientationOpts::default())` returns `tree` (non-empty, root_memory_title set), `load_bearing` (path+fan_in, len≤5), `recent` (commit subjects), `active_memory_titles`, `head`/`indexed_head`, `anchor` counts.

- [ ] **Step 2 — implement** (caller passes an `open_read_only` connection; `orientation` installs the scope view itself using `resolve_git_context(root)`):

```rust
pub struct Orientation {
    pub tree: crate::query::tree::DirTree,
    pub load_bearing: Vec<(String, u64)>,       // (path, fan_in), top 5
    pub recent_commits: Vec<String>,            // last ~5 subjects
    pub hot_files: Vec<String>,                 // few recently-changed source files
    pub active_memory_titles: Vec<String>,      // active, non-dir, capped ~5
    pub head: String, pub indexed_head: String,
    pub anchor: crate::index::AnchorHealth,
    pub total_files: u32, pub parser_failures: u64,
}
pub fn orientation(conn: &Connection, root: &std::path::Path) -> anyhow::Result<Orientation> {
    let (commit_sha, worktree_id) = crate::index::resolve_git_context(root);
    crate::index::install_scope_view(conn, &commit_sha, &worktree_id)?;
    let tree = crate::query::tree::dir_tree(conn, &Default::default())?;
    // load_bearing: repo_brief(conn, RepoBriefOptions{ mode: Spine, limit: 5, .. }) -> map to (path, fan_in)
    // recent_commits/hot_files: read from indexed git history tables (read-only)
    // active_memory_titles: SELECT m.title FROM repo_memories m WHERE status='active'
    //   AND id NOT IN (dir-memory ids already shown in tree) ORDER BY updated_at_ms DESC LIMIT 5
    // head/indexed_head: git_history::status(conn, root)
    // anchor: memory::anchor_health_counts(conn); total_files/parser_failures: scoped counts
    todo!("compose per comments")
}
```

(`resolve_git_context` and `install_scope_view` are `pub(crate)` in `index/` — confirm/raise visibility to `pub(crate)` if needed; both are core-internal.)

- [ ] **Step 3 — run PASS**; **commit**: `feat(query): read-only orientation composer`.

### Task C2: HookInput dispatch + SessionStart path

**Files:** `crates/rag-rat-cli/src/claude_hook.rs`

- [ ] **Step 1 — failing test**: SessionStart JSON `{"hook_event_name":"SessionStart","source":"startup","cwd":"<temp repo>"}` (NO `tool_name`) parses and produces a digest on stdout; `source:"resume"` → empty; non-rag-rat cwd → empty.

- [ ] **Step 2 — fix `HookInput`**: add `hook_event_name: Option<String>` and `source: Option<String>`; change `tool_name`/`tool_input` to `#[serde(default)]` (so SessionStart input — lacking them — deserializes).

- [ ] **Step 3 — branch `run_inner`**:

```rust
let input: HookInput = serde_json::from_str(&raw).ok().unwrap_or_default(); // already tolerant
match input.hook_event_name.as_deref() {
    Some("SessionStart") => session_start(&input),   // new
    _ => pretooluse(&input),                          // existing grep path, refactored out
}
```

`session_start`: allowlist `input.source` ∈ {startup,clear,compact} else return; `find_config(cwd)` None → return; if DB file absent → `print!("{}", header_and_index_not_built())`; else `let conn = IndexConnection::open_read_only(db)?; let o = query::orientation(&conn, root)?; print!("{}", format_digest(&o, &watcher_state(config)));`. All inside a function that returns `Ok(())` and where every `?`/error path prints nothing (wrap the body; on `Err`, return Ok). **No stray stdout.**

- [ ] **Step 4 — run PASS**; **commit**: `feat(cli): SessionStart dispatch in claude-hook`.

### Task C3: watcher probe + digest formatting

**Files:** `crates/rag-rat-cli/src/claude_hook.rs`

- [ ] **Step 1 — failing tests**: `format_digest` includes the attribution header, the LAYOUT tree (with a dir-memory title), `load-bearing … (fan_in N)`, and a watcher-aware health line; `gone>0` adds the `memory doctor` nudge; watcher-off + behind → `run 'rag-rat index'`, watcher-live + behind → `index syncing (watcher live)` (no reindex nudge).

- [ ] **Step 2 — watcher probe**:

```rust
fn watcher_state(config: &Config) -> (bool /*live*/, bool /*enabled*/) {
    let enabled = config.watch.enabled;
    // live iff the per-worktree election lock is currently HELD (try_lock fails to acquire)
    let live = matches!(
        crate::... /* FileLock::try_acquire on locks::election_lock_path(config) */,
        Ok(None)
    );
    (live, enabled)
}
```

Use the existing `locks` election-lock path + `FileLock::try_acquire` (non-blocking; `Ok(None)` = held by a watcher). Confirm the exact `locks` fn name for the per-worktree election lock and reuse it.

- [ ] **Step 3 — `format_digest`**: produce the plain-text digest exactly per the spec's §C3 example: attribution+nudge header; purpose = `tree.root_memory_title` (omit if None); `LAYOUT` from `tree.nodes` (indent by `depth`, `name  ‹memory_title›`, fold `truncated` into `… (+k more)`); `load-bearing: <path> (fan_in <n>) · …`; `recent: <subjects> · hot: <files>`; `memories: <titles> [+k more]`; `health:` via the watcher-aware table (live/syncing/stale/off) + `memory doctor` only when `gone>0` + parser-failures only when `>0`. Bound length.

- [ ] **Step 4 — run PASS**; **commit**: `feat(cli): orientation digest formatting + watcher-aware health`.

### Task C4: settings — register SessionStart

**Files:** `crates/rag-rat-cli/src/claude_settings.rs`

- [ ] **Step 1 — failing tests**: `install` writes a `SessionStart` entry (`matcher:"startup|clear|compact"`, command `rag-rat claude-hook`, `timeout:5`) AND keeps PreToolUse Grep/Bash; re-install is idempotent; changing the SessionStart matcher then re-installing **replaces** (no duplicate); uninstall removes both; foreign entries preserved; `status` reports both.

- [ ] **Step 2 — generalize**: parameterize the array-navigation helpers by `event_name`. PreToolUse keeps per-matcher entries. Add a SessionStart installer that ensures a single is-ours entry, detected by `is_ours` (not matcher equality) and replaced if matcher/timeout differ. Refactor `hook_status` `(bool,bool)` → a named struct `HookStatus { pretooluse: bool, session_start: bool }`. Add a `timeout` field to the SessionStart entry (PreToolUse entry already has `timeout:10`).

- [ ] **Step 3 — run PASS**; **commit**: `feat(cli): register SessionStart hook in settings (idempotent)`.

### Task C5: CLI wiring + usage

**Files:** `crates/rag-rat-cli/src/commands.rs`, `main.rs`

- [ ] **Step 1** — ensure `rag-rat hooks install|uninstall|status --claude [--global]` drives both events (via the generalized helpers); `status` prints both; `usage()` mentions the SessionStart orientation digest. **Step 2** — `cargo build --workspace`; **commit**: `feat(cli): hooks install/status cover SessionStart`.

### Task C6: Final gate + manual verify
- [ ] `cargo clippy --workspace --all-targets` clean; `cargo test --workspace` (ignore `sigusr1_*`); `cargo +nightly fmt --all`; update `crates/rag-rat-cli/tests/mcp_stdio.rs` only if it asserts the tool/hook set.
- [ ] Manual: author a root dir memory (`memory_create bind:{dir:""}`) + a couple of subsystem dir memories; `rag-rat hooks install --claude`; inspect `.claude/settings.json`; start a fresh session → confirm digest; `/clear` + a compaction re-inject; `--resume` does not.
- [ ] **VERIFY** (spec open item): does SessionStart fire on Task/subagent spawns? If yes, gate the digest (e.g., skip when a subagent indicator is present) so it doesn't inject into every subagent.

---

## Self-review notes

- Spec coverage: Phase A (dir memories + CLI read) → A1–A4; tree builder → B1–B2; hook dispatch/compose/format/settings/wiring → C1–C5; watcher-aware health → C3; Fable P1.1 (open_read_only) → C2/C1; P2.1 (scoping) → B1+C2; P2.3 (HookInput) → C2; P2.5 (absent DB) → C2; P2.6 (settings idempotency) → C4.
- Two `todo!()` scaffolds (B2 `dir_tree`, C1 `orientation`) are each resolved within their own task's next step — the bodies are described concretely; implementer must replace them (do not leave a `todo!`).
- Names consistent: `normalize_dir`, `dir_has_files`, `resolve_dir_binding`, `validate_dir_binding`, `install_scope_view`, `dir_tree`/`DirTree`/`TreeOpts`, `orientation`/`Orientation`, `watcher_state`, `format_digest`, `HookStatus`, `list_memories`/`MemorySummary`.
- Open tuning: tree pruning constants (B2 step 3); SessionStart-on-subagent verification (C6).
