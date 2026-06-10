# Spec: In-Place Hot Upgrade of rag-rat's stdio MCP server via SIGUSR1

Status: approved for implementation (build A + B). Uncommitted (per repo convention).
Source: user-provided general spec, **tailored to rag-rat** with verified rmcp facts + deviations.

## Why
A live `rag-rat mcp` process keeps running the OLD binary until the client reconnects (the recurring
"MCP holds the old binary" pain). On `SIGUSR1`, the process should `exec` the newly installed binary
in place — same PID, same stdio pipes, no re-`initialize`, no dropped/dup requests.

## Verified against pinned rmcp 1.7.0 (no rmcp upgrade needed)
- `rmcp::service::serve_directly(service, transport, Some(peer_info))` — **documented "skip
  initialization process"** (service.rs:688). Load-bearing assumption CONFIRMED.
- `serve_directly_with_ct` also available. `RunningService::cancel()` (service.rs:614 async),
  `peer_info()` (487), `peer()` (522). `IntoTransport for (R, W)` async (transport/async_rw.rs:24)
  → `(GatedStdin, tokio::io::stdout())` works.
- Type is **`InitializeRequestParams`** (plural), `rmcp::model` (model.rs:801) — NOT `…Param`.

## rag-rat-specific simplifications (vs the general spec)
rag-rat is a **read-only query server**: no resource subscriptions, no `roots` usage, no
`logging/setLevel`, no notifications, no sampling. Therefore:
- **Handoff is minimal**: just `peer_info` + negotiated protocol version (drop subscriptions /
  client_roots / log_level from the general spec).
- **In-flight guard is ONE chokepoint**: all ~34 tools funnel through `RagRatService::call` (and the
  `call`-based dispatch in `crates/rag-rat-mcp/src/tools.rs`). Guard there, not 30 handlers.
- **No `listChanged` machinery**: tool list is static across versions (if a future version adds
  tools, the hot-upgraded client keeps the old list until reconnect — acceptable; do NOT send
  unconditional listChanged).
- **No persistent SQLite/flock in the MCP process**: tools open `IndexDatabase::open_config` per
  call and drop it. The only long-lived resource is the **Watcher** (`core::watch::Watcher`), which
  holds the election lock + takes the write lock per pass. Teardown = **drop the Watcher** (releases
  its locks) — there is no separate DB/flock fd to close in the server.

## Deviations from the general spec (deliberate, simpler, equally correct for rag-rat)
- **Handoff via temp file + env PATH, NOT memfd.** Write `HandoffV1` to a temp file (e.g.
  `<db_parent>/handoff-<pid>.bin` or `$TMPDIR`), pass its path via `MCP_HANDOFF_PATH`, read + `unlink`
  + unset-env post-exec. Avoids all `memfd_create`/`FD_CLOEXEC`/raw-fd-inheritance code and is
  cross-Unix (memfd_create is Linux-only). **Only stdio fds (0/1/2) cross `exec`** (they lack
  O_CLOEXEC by default); nothing else needs to. Keep `residue: Vec<u8>` (always empty in v1).
- **flock**: the Watcher's locks are released by dropping the Watcher before exec; the resumed
  process re-spawns the Watcher which re-acquires (blocking) — matches the spec's default
  release-then-reacquire. No fd inheritance for locks.
- **INSTALL_PATH via env `RAG_RAT_UPGRADE_BIN`** (absolute path of the installed binary, e.g.
  `~/.cargo/bin/rag-rat`). If unset, hot-upgrade is **disabled** (SIGUSR1 logs a hint). Never
  `/proc/self/exe` (old inode). Phase B watches `dirname(RAG_RAT_UPGRADE_BIN)`.

## Components

### Gated stdin (`GatedStdin`)
`AsyncRead` wrapping `BufReader<Stdin>` + `pending: Arc<AtomicBool>` + `gate: Arc<Notify>` +
`partial_line: bool`. Behavior:
1. `!pending`: read normally; update `partial_line` (true unless last delivered byte was `\n`).
2. `pending && !partial_line` (line boundary): return `Poll::Pending`, park on `gate`; never start a
   new read → zero residue by construction. Unread requests stay in the kernel pipe buffer.
3. `pending && partial_line`: finish the current line, then park.
Transport: `(GatedStdin, tokio::io::stdout())`.

### In-flight tracking (`Inflight`)
Atomic counter + `Notify`; RAII guard increments/decrements; `wait_zero()` resolves when count==0
&& pending. Acquire the guard once, at the `call` dispatch in `tools.rs`/`server.rs`.

### Signal
`tokio::signal::unix::signal(SignalKind::user_defined1())`. Handler sets `pending=true` + notifies
gate; second SIGUSR1 while pending is a no-op.

### HandoffV1
```rust
#[derive(Serialize, Deserialize)]
struct HandoffV1 {
    format_version: u32,                  // = 1
    negotiated_protocol_version: String,
    peer_info: InitializeRequestParams,   // rmcp::model, serde-able
    residue: Vec<u8>,                     // always empty v1
    old_binary_inode: u64,
    upgrade_started_unix_ms: u64,
}
```
(JSON or postcard; version first.)

### Teardown order (run on the async runtime, not in the signal handler)
1. `pending=true`; gate closes at next line boundary.
2. `inflight.wait_zero().await` bounded by **drain timeout** (default 30s). On timeout → ABORT:
   reopen gate, clear pending, log loudly, keep serving on old binary.
3. flush stdout.
4. snapshot `HandoffV1` (peer_info from `service.peer().peer_info()`), write temp file, set
   `MCP_HANDOFF_PATH`.
5. `service.cancel().await`.
6. `drop(watcher)` (releases election/write locks).
7. `Command::new(install_path).args(env::args_os().skip(1)).env("MCP_HANDOFF_PATH", path).exec()`.
   `exec` only returns on failure → log errno, exit non-zero (client sees EOF, relaunches).

### Resume (post-exec startup in run_stdio)
- If `MCP_HANDOFF_PATH` set → read+deserialize+unlink+unset.
  - **Protocol-version gate**: if the new binary doesn't support `negotiated_protocol_version`, do
    NOT resume — exit cleanly (client reconnects + renegotiates).
  - else `serve_directly(RagRatService::new(config), (gated_stdin, stdout()), Some(peer_info))`.
- Else cold: `RagRatService::new(config).serve((gated_stdin, stdout()))`.
- Re-spawn the Watcher (already done in run_stdio) → re-watches source dirs AND (Phase B) the binary
  dir, so the chain works for the next upgrade.
- MUST NOT expect `initialize`, MUST NOT send unconditional `listChanged`.

## Phase B — fleet trigger (in the elected watcher)
The elected watcher (one per worktree) ALSO watches `dirname(RAG_RAT_UPGRADE_BIN)` for
`IN_MOVED_TO` of the binary filename (atomic `mv` from `cargo install`), debounce 500ms. On trigger:
- capture old inode of INSTALL_PATH (before/at rename); enumerate `/proc/*/exe`, select PIDs whose
  exe resolves to the **old inode** (precise; not `pkill -x` by name); `kill(pid, SIGUSR1)` each,
  excluding self; then set own `pending`. No barrier — each upgrades at its own boundary; blocking
  lock re-acquire + SQLite busy_timeout (already set) absorb the stampede.
- Linux-only (`/proc`); gate behind `#[cfg(target_os = "linux")]` with a no-op elsewhere.

## Failure modes (required)
- Drain timeout → abort, keep serving old binary.
- exec fail → exit non-zero (torn down), client reconnects.
- Handoff deserialize fail post-exec → treat as cold start OR exit to force reconnect (pick + log).
- Protocol mismatch → clean exit, no resume.
- SIGUSR1 before initialize completes → defer until handshake done, then upgrade at next boundary
  (or exec with NO handoff and let the new process handle the pending initialize from the pipe).
- Two SIGUSR1 → idempotent.

## Acceptance tests (subprocess harness)
1 transparent upgrade (no reconnect/initialize, PID unchanged) · 2 boundary half-line · 3 in-flight
drain (slow tool answered by old) · 4 pipe-buffer carryover (3 sent, signal after 1 read, remaining
2 answered in order once) · 7 drain timeout abort · 8 exec failure → non-zero exit · 9 version gate ·
10 old-inode targeting (same-name different-inode process NOT signaled) · 6 fleet (N processes,
shared DB+locks, atomic mv, all upgrade, no deadlock/SQLITE_BUSY, watcher upgrades last).
(5 subscription-continuity is N/A — rag-rat has no subscriptions.)

## Implementation tasks
- #9 handoff + gated stdin + inflight
- #10 SIGUSR1 teardown + exec + resume (wire run_stdio)
- #11 Phase B fleet trigger (binary watcher + /proc inode targeting)
- #12 acceptance tests

## Status of code so far
Implemented (uncommitted):
- `rag-rat-mcp/src/upgrade.rs` (`#![cfg(unix)]`) — `HandoffV1` (temp-file round-trip), `GatedStdin`
  (one-line-at-a-time, parks at boundary), `Inflight` (+ guard / `wait_zero`), `UpgradeGate`,
  install-path/handoff/version-gate helpers, and `Upgrade::run` (drain → flush → handoff → `exec`,
  abort on drain timeout, `exit(70)` on `exec` failure).
- `rag-rat-mcp/src/server.rs` — `RagRatService` carries the in-flight counter (guarded at the one
  `call` chokepoint); `run_stdio_unix` serves over `GatedStdin`, resumes via `serve_directly` on a
  valid handoff (clean exit on protocol mismatch), and arms the SIGUSR1 task when
  `RAG_RAT_UPGRADE_BIN` is set.
- `rag-rat-core/src/fleet.rs` — Linux `/proc` fleet trigger: targets only our-binary + `mcp` +
  hot-upgrade-armed (environ carries `RAG_RAT_UPGRADE_BIN`) + outdated-inode processes; self last.
  Pure `select_targets` unit-tested.
- `rag-rat-core/src/watch.rs` — `Watcher::spawn_with_fleet`; the elected watcher also watches the
  binary dir and calls `fleet::trigger` (500ms debounce) when a new binary lands.
- Tests: unit (gated-stdin line-split + park/resume, inflight drain, handoff round-trip, version
  gate, fleet target selection) + subprocess acceptance (`tests/mcp_hot_upgrade.rs`: transparent
  in-place resume; exec-failure non-zero exit).

Deviation taken vs the "## Teardown order" list above: teardown does **not** explicitly
`service.cancel()` or drop the watcher. `exec` replaces the process (stdio fds 0/1/2 inherited →
client pipe stays open; watcher lock fds are CLOEXEC → released automatically; SQLite WAL recovers
any interrupted pass). This avoids a bounded-but-slow watcher join in the teardown path and avoids
`unsafe set_var` (handoff path rides `Command::env`, never our own env).

The file watcher (separate feature) is done and squashed at commit `f287970` (unpushed).
