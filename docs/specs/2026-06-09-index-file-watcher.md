# Per-Worktree Index File Watcher

Status: design (approved in brainstorm 2026-06-09)
Decisions: MCP-embedded watcher + reusable core; on by default with an off-switch; **two** locks
(per-worktree watcher election + per-DB write serialization) via std's native `File::lock`/`try_lock` (stable 1.89, cross-platform — no crate);
one shared DB per repo (relative `database` resolves to the main worktree). All rag-rat state lives
under `<main>/.rag-rat/` (gitignored).

## Context

Today the index only refreshes on explicit `rag-rat index` or the git hooks
(post-checkout/merge/rewrite/commit). Between an edit and the next of those, lookups against
uncommitted edits are only partly usable:

- `search` / `read_chunk` auto-heal stale files lazily (`search_with_heal`, the `read_chunk` heal
  path), but heal is per-result-file and does not re-run global edge resolution.
- `symbol_lookup` / `find_callers` / `trace_callees` / `impact_surface` have **no heal step** and
  return whatever was last indexed — stale until a reindex. (This is the root cause behind #47.)

`index --watch` is reserved but unimplemented (`main.rs` bails). There is no file-watcher today.

This spec adds a background watcher that keeps the **active** index (including the dirty-worktree
overlay) fresh as files change, so every query — graph included — reflects live edits without a
commit. SQLite is WAL (`storage.rs`), so concurrent readers are safe; the only thing that must be
singular is the **writer**, which this design elects per index database.

## Goals

1. On file changes in configured target dirs — **additions, edits, renames, and deletions** —
   reindex + reconcile the active worktree promptly (new files indexed, deleted files removed).
2. Exactly one writer per index database, regardless of how many `rag-rat` processes (sessions,
   subagents, concurrent Claude Code instances, git worktrees of one repo) point at it.
3. Cross-project isolation: a watcher for project A never blocks or touches project B.
4. Zero new ceremony: runs inside `rag-rat mcp` by default; reusable as a standalone command.

## Non-goals

- No HTTP/daemon transport (separate, larger initiative; Claude Code has no unix-socket transport,
  and the axum/hyper stack fights the binary-size budget — see Rejected).
- No replacement of the git hooks; they remain a fallback for non-MCP / plain-git-CLI use.
- No on-demand heal for graph queries (a partial band-aid; the watcher makes it unnecessary).

## Design

### Two locks: watcher election (per worktree) + write serialization (per DB)

The earlier draft used one DB-keyed lock for both, which conflates two different concerns and
leaves a coverage hole. They are split:

**Intended model: one database per repo, shared across all its worktrees.** This is what the
commit-addressable overlay (`worktree_id`/`commit_sha`) and the "keep all live worktrees' HEADs" GC
were built for — reuse rows across branches/worktrees instead of N redundant indexes.

> **Prerequisite — IMPLEMENTED (`Config::load`):** a relative `database` resolves against the
> **main worktree root**, derived from `git rev-parse --git-common-dir` (`<main>/.git` → parent
> `<main>`). So **both** the main worktree and every linked worktree resolve to
> `<main>/.rag-rat/index.sqlite` — one shared DB. Critically this is keyed on the *main root*, not
> the common dir, so the resolution is **unconditional and uniform** (no "main vs linked" split that
> would silently produce two DBs). The main worktree / non-git / absolute-path cases resolve exactly
> as before, so **single-worktree installs see no change and no migration**; only linked worktrees
> move (their old per-worktree DB is orphaned and re-indexed once on first use — a stated, accepted
> behavior, not a surprise). `gc` runs `git worktree list` (which works from any worktree) to
> enumerate all live worktrees per the scoping invariant below.

**Watch coverage is per worktree, because the DB is shared.** Each `rag-rat` process watches **its
own worktree's** target dirs. With one shared DB, a single DB-keyed "one writer" lock would elect a
single watcher that only sees *its* worktree and leaves the others' dirty-overlay edits stale —
exactly the staleness this spec kills. So coverage must never be gated by the write lock.

1. **Watcher-election lock — keyed by the worktree root.** Lockfile lives under the shared index
   directory (the DB's parent, `<main>/.rag-rat/locks/<hash-of-worktree-root>.lock`), not the
   per-worktree tree. The root is **`std::fs::canonicalize`d before hashing** (resolves symlink
   aliases like `/tmp`→`/private/tmp` to one key). We deliberately do **not** case-fold: on a
   case-sensitive volume, folding could collapse two genuinely-distinct worktrees into one key and
   leave one **permanently un-elected** (silent staleness — the exact failure we're preventing). The
   only residual edge — the same checkout reached via differently-cased paths on a case-insensitive
   FS — merely elects two watchers, which the write lock makes harmless. Ensures **one watcher per
   worktree**; non-blocking acquire; losers re-attempt (~5 s) so a watcher is re-elected if a holder
   dies. (`.rag-rat/` is gitignored and is never a watch target, so locks/DB there don't self-trigger
   the watcher.)

2. **Write-serialization lock — keyed by `(canonical DB path)`.** Taken by **every writer** so
   "exactly one writer at a time per database" is literally true (WAL handles readers). Acquisition
   policy differs by caller:
   - **Watcher passes:** blocking — watcher-to-watcher should serialize, not skip.
   - **Hook (`maintenance`) and manual `index`:** **block** on the lock (IMPLEMENTED). The git hook
     backgrounds `rag-rat maintenance` (`… &`), so blocking never holds up the git operation, and
     manual `index` is user-initiated. Blocking (not timeout-skip) is deliberate: a skip would be
     *unsafe when the watcher is disabled* (`enabled=false`/`RAG_RAT_NO_WATCH`), the very config
     where hooks are the only freshness mechanism — a skipped hook would never be covered. The
     bounded-pass reconcile cap keeps the wait short; the only way blocking hangs is a *wedged*
     watcher (see Known limitations). The **watcher's own shutdown** pass is the exception — it uses
     timeout-skip (and discover-only), since the next startup catch-up covers a skip.
   - **Heal writers (`search_with_heal`, the `read_chunk` heal path) — the other door:** these write
     to the DB from the **MCP query thread**, so they are writers too. They take **no file lock** —
     instead `PRAGMA busy_timeout = 5000` (IMPLEMENTED in `storage.rs`) makes a concurrent writer
     *wait out* the watcher's in-flight SQLite write rather than erroring or corrupting. This is
     deliberately simpler and safer than file-locking heal: it sidesteps the **self-deadlock** that
     would arise if a heal on the query thread tried to blockingly re-lock the write lock the
     watcher thread already holds (file locks are per open-file-description, so a same-process
     re-lock waits forever). Heal stays the watcher-off fallback; with the watcher on it's largely
     redundant. (A WAL writer transaction is brief, so heal's wait is short, not a full pass.)

Locking uses **std's native `File::{lock, try_lock, unlock}`** (stable since Rust 1.89 — `flock` on
Unix, `LockFileEx` on Windows, no external crate; `rust-version` bumped to 1.89), **not** raw `libc`
(Unix-only). The OS releases the lock on process death, so there is no stale-pidfile cleanup.
**Caveats:** file locks are unreliable on NFS and on WSL2 `drvfs`/`9p` mounts (`/mnt/c/...`);
documented as a known limitation (a repo on a native filesystem is the supported case).

### Catch-up passes (no missed edits)

A debounced pass only covers edits it *observed*. To close the gaps, run an **unconditional full
discover pass**:

- on **MCP startup / watcher start** — catches everything that changed while no session was open;
- on **watcher election win** — catches edits during the ~5 s gap between a holder dying and a new
  one acquiring;
- when `notify` reports a **rescan / event-queue overflow** — treat the overflow flag as "trigger a
  pass" (free, since every pass is already a full-tree discover).

The pipeline is idempotent, so an extra sweep is always safe.

### Worktree-scoped destructive operations (hard invariant)

With one shared DB, a pass from worktree A must **never** remove rows owned by worktree B (B is on
another branch, or has uncommitted files that don't exist in A). Otherwise A deletes B's live
overlay, B's next pass deletes A's, and the two watchers slowly destroy each other — surfacing as
flaky staleness, not a crash.

**Verified current behavior (must be preserved and tested):** this is *already* scoped, and the
spec now pins it as a requirement:
- `discovery_plan`'s deleted set comes from `indexed_file_map`, whose `SELECT … FROM files`
  resolves to the **`temp.files` active view** (this worktree's overlay ∪ its commit), not
  `main.files` — so only the caller's own files are deletion candidates.
- `mark_file_deleted` → `remove_file_in_scope(path, "", active_worktree_id)` scopes the write to the
  caller's `worktree_id`.
- `gc` keeps every row whose `worktree_id` is a live worktree per `git worktree list` (run from the
  git **common dir**, so it sees all worktrees) — covering other worktrees' **dirty overlays**, not
  just their HEAD commits. Note: `git worktree list` includes **removed-but-unpruned** worktrees, so
  a deleted worktree's rows survive until `git worktree prune` runs — expected, not a leak (document
  it so post-deletion disk growth isn't mistaken for one). **Do NOT** have gc sweep orphaned
  election lockfiles: unlinking a file another process holds an `flock` on is a footgun (the holder
  keeps its lock on the now-anonymous inode, the next candidate creates a fresh file at the same
  path and also locks it → two elected watchers, recreated every gc cycle). The few-byte lockfiles
  are left alone.

**Requirement:** every destructive op in a pass (file removal, gc) is eligible only for rows the
invoking `worktree_id` owns, judged against that worktree's filesystem view. A refactor that points
deletion at `main.files` (global) reintroduces the mutual-destruction bug — so this needs an
explicit unit test: a DB seeded with two `worktree_id`s, a pass run as one, asserting the other's
rows survive (including a file that exists only in the other worktree).

### Watch loop

- Use the `notify` crate (cross-platform: inotify / FSEvents / ReadDirectoryChanges).
- **Watch the configured target *directories* recursively** (`RecursiveMode::Recursive`), not the
  set of currently-indexed files. This is what makes **newly added files** observable: a file that
  does not exist at index time has no inotify watch of its own, so the watch must be on the
  enclosing dir tree and react to `Create` events. Ignore `.git/`, `target/`, `.rag-rat/`,
  `node_modules/`, and anything outside the targets.
- **Glob classification only decides *whether to fire a pass*, not *what to index*.** Each event is
  matched against the target include/exclude globs; an event under an ignored path (or not matching
  any target) is dropped so it doesn't trigger a pass. The pass itself is a full discover sweep that
  decides what to index/remove (see below) — classification is the trigger filter, not the indexer.
- **New subdirectories:** `notify`'s inotify backend in recursive mode *does* auto-add watches for
  newly created dirs — so no manual re-registration is needed. The real (small) hole is the race
  where files are written into a brand-new dir before its watch lands, dropping those individual
  events. The discover sweep covers exactly that, so the conclusion holds; no extra re-watch step.
- **Debounce with a max-latency cap**: coalesce events after a quiet window (default ~400 ms,
  `[watch] debounce_ms`), **but force a pass after a hard cap of continuous activity** (default
  ~2.5 s, `[watch] max_latency_ms`). A pure quiet-window debounce never fires under sustained writes
  (codegen loop, a misconfigured exclude that keeps matching); the cap guarantees progress.
  **Hand-roll this (don't swap in `notify-debouncer-full`)** — the off-the-shelf debouncer gives the
  quiet window but **not** the max-latency cap, so a naive "simplification" to it loses
  starvation-protection. Note that explicitly so nobody removes it.
- On each debounced batch, take the **per-DB write-serialization lock (blocking)** and run the
  **existing maintenance pipeline** — do not reinvent it: `index_discover → reconcile
  (changed-first, bounded) → gc → memory_validate`. Discover mode is what makes this complete: it
  **adds new files, re-indexes changed files, and removes deleted files** across the target tree
  (not just refreshing already-known files). The pipeline is idempotent; release the lock after.
  - **Rate-limit `gc` inside the watcher** (e.g. every N passes or every few minutes), not every
    pass. During active editing a pass fires every ~400 ms–2.5 s, and `gc` shells out to
    `git worktree list` + a full liveness scan each time — needless, since deletion reconciliation
    is already handled by discover's deleted-set. Hooks / manual `index` still run the full pipeline
    including `gc` every time.
- **Cost notes (two levers, measure both):**
  - **Lock-hold duration is the likelier first pain point.** A pass currently holds the per-DB write
    lock through `index_discover → reconcile → gc`, and `reconcile` includes **embedding** — slow
    model inference that is pure computation needing no serialization. That's what forces
    hook-timeout-and-skip and what makes two worktrees' watchers serialize on each other's inference
    time. **Structural fix (deferred past v1):** discover + embed *outside* the lock (reads are
    WAL-safe), take the lock only around the DB mutation; the embed→write TOCTOU is harmless given
    idempotence + the next pass. For v1, keep it simple but **measure lock-hold time in the
    two-worktree e2e**, not just correctness.
  - **`O(tree)` sweep.** Every pass stats the whole target tree (matches the git hooks). Likely fine
    at cq27-scale; if it bites, feed the classified changed-path set through a targeted fast path
    (`index --changed`) with a periodic full discover for new/deleted files. Out of scope for v1.
- **Self-trigger guard (must-hold invariant):** every write the pipeline makes must land **outside
  every watched target dir**, or the watcher feeds itself. All rag-rat state lives under
  `<main>/.rag-rat/` (the shared index dir: DB, write-lock, election locks) — which is **gitignored
  and never a watch target** (targets are source dirs like `src/`/`docs/`) — or `~/.cache/rag-rat`
  (embedder caches). `memory_validate`/`gc` write only to the DB. The eval check confirms a single
  edit settles rather than looping.
- Run off the MCP query path (dedicated thread / task) so queries stay responsive; the writer and
  readers coordinate only through WAL + the DB. **On shutdown, attempt one final pass** so an edit
  in the last debounce window isn't deferred — but **discover-only (no embedding), timeout-skip**:
  the host may `SIGKILL` shortly after stdin EOF, so shutdown must be bounded; discover is fast and
  keeps structure fresh, and the next startup catch-up does the embedding. (IMPLEMENTED.)
- **Atomic-save editors** (write-temp-then-`rename`, e.g. vim's `4913` dance) are handled: event
  classification matches **any** path on an event (rename destinations included), not just
  create/modify, so the rename-to-`foo.rs` fires a pass and discover indexes it.

### MCP embedding

- `rag-rat mcp` startup spawns the watcher (after opening the DB / resolving the worktree). On a
  clean shutdown it stops the watcher and drops the lock; on a crash the OS drops the lock.
- **Startup is non-gating:** the catch-up pass runs **async on the watcher thread**; `run_stdio`
  serves queries immediately against the (possibly one-pass-stale) DB. This is the same
  stale-for-one-pass contract queries already have — startup never blocks on a reindex sweep.
- **Shutdown is driven by stdin EOF, not signals.** We don't know how Claude Code terminates stdio
  servers (it may `SIGKILL`), so don't rely on signal handlers / `Drop` for the final pass. The
  graceful signal we *do* control is the stdin pipe closing: when `run_stdio` sees EOF, treat that
  as shutdown and trigger the (timeout-and-skip) final pass from there.
- The watch logic is a reusable core module so a future `rag-rat watch` command (CI / headless) and
  the now-implementable `index --watch` share it.

### Config / off-switch (on by default)

```toml
[watch]
enabled = true        # default; set false to disable the background watcher
debounce_ms = 400     # quiet window before a reindex pass
max_latency_ms = 2500 # force a pass after this much continuous activity
periodic_sweep_secs = 300 # backstop: pass at least this often (0 disables); covers blind FS
```

- `RAG_RAT_NO_WATCH=1` also disables it (for CI / sandboxes), taking precedence.

## Affected files

- `crates/rag-rat-core/src/watch.rs` (new) — `notify` watcher, debounce + max-latency, the two
  locks (per-worktree election, per-DB blocking write lock), catch-up passes, and the per-batch call
  into the maintenance pipeline. Reusable core.
- `crates/rag-rat-core/src/locks.rs` (new) — std-native file-lock helpers used by the watcher **and** by the writer paths below.
- `crates/rag-rat-core/src/config.rs` — `[watch]` section (`enabled`, `debounce_ms`,
  `max_latency_ms`), defaults. Plus the DB-path prerequisite (resolve to git-common-dir) — may be a
  separate change.
- `crates/rag-rat-core/src/index/mod.rs` — the `index`/`maintenance`/`gc` write entry points take
  the per-DB write lock (blocking) so all writers serialize, not just the watcher.
- `crates/rag-rat-mcp/src/server.rs` — spawn/stop the watcher around `run_stdio`.
- `crates/rag-rat-cli/src/main.rs` — implement `index --watch` via the shared core; the git-hook
  `maintenance` path and `index` already serialize via the write lock; optional `rag-rat watch`.
- `Cargo.toml` — add `notify` (FS events) only; locking uses std (Rust 1.89, `rust-version` bumped). **No `fs4`** — std stabilized cross-platform file locks. Record the size delta per
  `docs/binary-size.md`.
- `README.md` / `docs/config.md` — document `[watch]`, the off-switch, the one-DB-per-repo model,
  the writer-serialization guarantee, and the NFS/WSL lock caveat.

## Verification

1. `cargo test -p rag-rat-core` — unit tests:
   - debounce coalesces a burst into a single pass; the **max-latency cap forces a pass** under
     sustained events that never reach a quiet window;
   - **watcher-election lock** (per worktree): a second process on the *same* worktree+DB does not
     win election; two different worktrees each win (both watch);
   - **write-serialization lock** (per DB, blocking): concurrent passes serialize — no interleaved
     writes; the hooks/`index` paths take the same lock;
   - locks auto-release on holder drop → a waiter then acquires; cross-project (different DB) →
     independent;
   - **worktree-scoped deletion** (the item-1 invariant at unit level): a DB seeded with two
     `worktree_id`s, a discover/gc pass run as one, asserts the other's rows survive — including a
     file that exists only in the other worktree;
   - **heal under contention**: with a writer active, `search_with_heal` / `read_chunk` heal wait
     out the writer via `busy_timeout` (no error, no self-deadlock — heal takes no file lock);
   - interactive/hook acquisition **times out and warn-skips** instead of blocking forever;
   - `[watch] enabled = false` / `RAG_RAT_NO_WATCH` → no watcher starts.
2. Manual end-to-end, all uncommitted: with `rag-rat mcp` running —
   - edit a file → `find_callers`/`symbol_lookup` reflect the edit within ~debounce + reindex time;
   - **create a new file** matching a target → it becomes searchable / its symbols resolve;
   - delete a file → its symbols/chunks drop out;
   - create a file under an ignored path or non-matching glob → it is **not** indexed;
   - **two git worktrees of one repo sharing one DB, edited simultaneously** → both worktrees'
     edits land, writes don't corrupt, and neither worktree goes stale (this is the test that
     exposes the coverage hole from review item 1, so it must use two *worktrees*, not two sessions
     in one checkout).
3. `rag-rat eval` — current-source violations stay at zero; confirm the watcher does **not**
   self-trigger (a single edit settles to a quiescent state, not a reindex loop).
4. Size check per `docs/binary-size.md` (`notify` only; locking is std, no dep).

## Rejected / deferred

- **HTTP/WS daemon (one shared process per worktree).** Confirmed feasible host-side (Claude Code
  supports `type: "http"`/`"ws"`), and it would make the writer a natural in-process singleton and
  allow a shared warm embedder. But: localhost-TCP only (no unix-socket transport in Claude Code →
  network surface), axum/hyper/tower size cost against the slim-binary budget, and self-owned daemon
  lifecycle (discovery, idle shutdown, stale cleanup). Heavier; revisit only if cross-session
  resource sharing (single embedder/cache) becomes the goal.
- **Per-process lock.** Insufficient: concurrent Claude Code instances / inline-MCP subagents can
  spawn multiple `rag-rat` processes on one DB; the lock must be cross-process, keyed by the DB.
- **On-demand heal for graph queries.** `heal_index` rescans every file and doesn't re-resolve
  edges (found during #47), so it's a partial fix; the proactive watcher supersedes it.

## Known limitations

- **Event-blind filesystems (#7).** On NFS and WSL2 `drvfs`/`9p` (`/mnt/...`), both `flock` *and*
  `inotify` are unreliable — a watcher can win election, run its startup catch-up, then never see
  another event. Mitigation (IMPLEMENTED): the **periodic sweep** (`[watch] periodic_sweep_secs`,
  default 300) fires a pass on a timer regardless of events, so the watcher is at worst
  `periodic_sweep_secs` stale on these mounts. Keep repos on a native filesystem for event-driven
  freshness; the git hooks remain a fallback.
- **Wedged-but-alive watcher (#6).** A hung (not dead) elected watcher holds the election lock until
  the OS reaps it on process death; losers poll but don't steal (stealing an `flock`'d lockfile is
  unsafe — see the gc note). A wedged holder mid-pass also holds the write lock, so blocking writers
  wait on it. v1 does not add a heartbeat/steal mechanism; the periodic sweep bounds staleness for
  the *non-wedged* missed-event case but not a truly wedged thread. Documented; a heartbeat-warning
  (holder stamps a timestamp; losers warn on staleness) is a candidate follow-up.

## Open validation item

The Claude Code docs do not specify stdio process lifecycle or concurrent-instance behavior
(per claude-code-guide). The two-lock design is robust to all of it by construction, but the
concurrent-writer/coverage path must be validated in practice with **two git worktrees of one repo
sharing one DB, edited simultaneously** (not two sessions in one checkout) — that is the
configuration that exposes review item 1. Also validate that `index --watch` / hooks / manual
`index` running concurrently serialize cleanly through the write lock.
