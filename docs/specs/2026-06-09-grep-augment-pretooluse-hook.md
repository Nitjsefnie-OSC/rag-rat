# Spec: Grep-Augmentation PreToolUse Hook (Claude Code)

Status: design (approved in brainstorm 2026-06-09). Uncommitted (per repo convention).
Implementation target: separate worktree.

## Why

Agents grep. Every `Grep`/`rg` sweep is a moment where rag-rat already knows more than the
text hits: where the symbol actually lives, who calls it, and — uniquely — whether a repo
memory (invariant/decision/risk) is bound to the code being searched. Today that knowledge
only reaches the agent if it *chooses* to call the MCP tools. A PreToolUse hook makes it
ambient: the grep still runs untouched, and the model additionally receives a compact
rag-rat digest as `additionalContext`. This is the "drive-by memory" delivery channel,
competitive with codebase-memory-mcp's grep-augmentation hooks but powered by the memory +
provenance layer they don't have.

## Verified hook contract (Claude Code docs, 2026-06-09)

- PreToolUse hooks receive stdin JSON: `session_id`, `cwd`, `hook_event_name`, `tool_name`,
  `tool_input` (`{pattern, path, …}` for Grep; `{command}` for Bash).
- Exit-0 stdout JSON may carry `hookSpecificOutput: {hookEventName: "PreToolUse",
  permissionDecision: "allow", additionalContext: "…"}` — the tool call **proceeds** and the
  model **sees** `additionalContext`. Plain stdout is debug-log-only for PreToolUse; only the
  JSON field reaches the model.
- Matchers: `"Grep"`, `"Bash"` under `hooks.PreToolUse` in `.claude/settings.json` (project)
  or `~/.claude/settings.json` (global). Per-hook `timeout` in seconds.
- A hook that times out or exits non-zero (other than 2) does not block the tool call. We
  never exit 2.

## Grounding in current code (verified)

- CLI is a flat match over subcommands (`crates/rag-rat-cli/src/main.rs:31-135`); `hooks`
  has `install|uninstall|status` over `MANAGED_HOOKS` git hooks with marker-checked
  uninstall (`main.rs:559-628`); git hooks dispatch back into `rag-rat maintenance
  --trigger …` (`main.rs:766+`).
- Watcher election: `locks::election_lock_path` (`crates/rag-rat-core/src/locks.rs:92`),
  `FileLock::try_acquire` (`locks.rs:43`), retry loop "so a new watcher takes over if a
  holder dies" (`crates/rag-rat-core/src/watch.rs:209-218`).
- MCP server: `run_stdio` → Unix path spawns `Watcher::spawn_with_fleet` and serves rmcp
  over `GatedStdin` (`crates/rag-rat-mcp/src/server.rs:452-507`); `RagRatService` holds
  `Config` + an `Inflight` counter (`server.rs:26-43`).
- Query entry points to reuse: `query::symbol::lookup` / `lookup_candidates`
  (`crates/rag-rat-core/src/query/symbol.rs:73,84`), `query::memory::memory_search`
  (`query/memory.rs:497`), `query::memory::memories_for_symbol` (`query/memory.rs:346`),
  lexical search `search::lexical::search_with_options` (`search/lexical.rs:107`).
- `Storage::open` (`crates/rag-rat-core/src/storage.rs:24`) — no read-only variant yet; the
  fallback path adds one.

## Decisions (from brainstorm Q&A)

1. **Payload**: adaptive — symbol hits + bound memories when the pattern resolves; memory
   FTS always; top-3 lexical hits as fallback. Never load the embedding model.
2. **Scope**: intercept the `Grep` tool and Bash invocations of `grep`/`rg`/`ag`.
3. **Noise**: per-`session_id` dedupe, server-side in memory.
4. **Install**: `rag-rat hooks install --claude` (project `.claude/settings.json`), optional
   `--global` (`~/.claude/settings.json`).
5. **Transport**: hook talks to the running MCP server over a Unix domain socket
   (user-selected approach B), with a direct read-only SQLite fallback when no listener.

## Architecture

Three components:

### 1. Hook client — `rag-rat claude-hook`

New flat CLI subcommand (dispatch-style, like `maintenance`). Flow per invocation:

1. Read the PreToolUse JSON from stdin.
2. Resolve the repo: walk up from the hook's `cwd` to the nearest `rag-rat.toml`. None
   found → print nothing, exit 0 (this is what makes `--global` install safe).
3. Extract a search intent:
   - `tool_name == "Grep"` → `tool_input.pattern` (+ optional `path`).
   - `tool_name == "Bash"` → conservative command-line parse (below); no match → exit 0.
4. Try the socket (connect + request + response, total budget ~250 ms). On any failure —
   no socket file, refused, timeout, bad response — fall back to a direct read-only query
   in-process.
5. If the payload is non-empty, print
   `{"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow",
   "additionalContext": <digest>}}`. Otherwise print nothing.
6. Always exit 0. Every error path is silent (stderr only under `RAG_RAT_HOOK_DEBUG=1`).

### 2. Socket listener — in `rag-rat mcp`

- Unix domain socket at `<db parent>/sockets/<worktree-hash>.sock`, sibling of the existing
  `locks/` dir, hashed the same way as `election_lock_path`. If the resulting path exceeds
  the ~108-byte `sun_path` limit, fall back to `$XDG_RUNTIME_DIR/rag-rat/<hash>.sock`, then
  `std::env::temp_dir()`.
- **Ownership by election**, mirroring the watcher: a second lock file
  (`locks::socket_lock_path`, new helper beside `election_lock_path`) with the same
  `try_acquire` + retry loop. The socket election is deliberately *separate* from the
  watcher election: it keeps core (election lives in `rag-rat-core::locks`) from calling
  back into `rag-rat-mcp`, and it doesn't matter if a different process wins each — both
  serve the same shared DB.
- Only the lock holder ever unlinks a pre-existing socket path before binding — the lock
  makes stale-socket cleanup race-free (a crashed owner's leftover file is removed by the
  next winner, never by a non-owner).
- Listener task lives in `rag-rat-mcp` (new module `claude_hook.rs`), spawned from
  `run_stdio` alongside the watcher. Tokio `UnixListener`; connections are served
  **serially** in the accept loop (the per-session dedupe map is owned by the loop as
  `&mut`, so serializing is what keeps it borrow-safe without a mutex). A per-connection
  read budget (500 ms) ensures a stalled peer cannot wedge the loop; the actual DB read runs
  in `spawn_blocking`. Each request opens its own read-only connection (WAL allows concurrent
  readers; the listener issues reads only).
- **Session dedupe state**: `HashMap<session_id, InjectedSet>` where `InjectedSet` records
  memory IDs and symbol-summary keys already sent. LRU-capped (64 sessions) with TTL
  (24 h); both are internal constants, not config. Dedupe means: drop already-sent memories/symbol summaries from the payload;
  a fully-deduped payload returns `context: null`.
- **Hot-upgrade interaction**: the listener (with its election lock and bound socket) drops
  on teardown like the watcher does today; the post-`exec` process re-runs both elections
  and re-binds. Locks release on process exit by construction (`FileLock` is fs-backed).
- Non-Unix: no listener; the hook client always uses the fallback path (consistent with
  fleet/hot-upgrade being Unix-only).

### 3. Fallback path — direct read-only SQLite

- `Storage::open_read_only` (new; `rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY |
  SQLITE_OPEN_NO_MUTEX`). WAL databases serve read-only opens fine once `-shm` exists; if
  the open fails (e.g. fresh DB never opened for write), emit nothing.
- Shares the exact payload-composition code with the listener (both call the same
  `rag-rat-core` function). No dedupe — stateless. Documented consequence: during
  listener-handoff windows or with no server running, repeats are possible; bounded,
  token-cost-only.

## Wire protocol (socket)

Newline-delimited JSON, versioned, one request per connection:

```json
→ {"v":1,"kind":"grep_augment","session_id":"…","pattern":"…","search_path":"…|null","source":"grep_tool|bash"}
← {"v":1,"context":"…digest…"}        // or {"v":1,"context":null}
```

Unknown `v` or `kind` → listener replies `{"v":1,"context":null}`. The client treats any
malformed reply as "fall back". Protocol lives next to the listener in `rag-rat-mcp`;
client and server are always the same binary version in practice (single installed binary;
hot-upgrade converges fleet members), so no cross-version negotiation beyond the `v` field.

## Payload composition (shared, in `rag-rat-core`)

New module `query::grep_augment` with one entry point taking `(conn, pattern, search_path,
dedupe_filter)` and returning a rendered digest + the IDs it included (for dedupe
bookkeeping):

1. **Normalize** the pattern: strip regex metacharacters/anchors; trim. If the residue
   matches an identifier shape (`[A-Za-z_][A-Za-z0-9_:.]*`, length ≥ 3), treat as symbol
   query.
2. **Symbol lane** (identifier shape only): `query::symbol::lookup` exact-name only (no fuzzy
   `lookup_candidates` pass — the hook fires on every grep, so the lane stays conservative to
   keep noise/latency down; a capped fuzzy fallback is a possible future addition), capped at
   3 symbols. Per symbol: `file:line`, kind, one-line signature, caller/callee counts (cheap
   aggregate over `edges`), and bound memories via `memories_for_symbol`.
3. **Memory lane** (always): `memory_search` FTS on the normalized pattern, plus path-bound
   memories when `search_path` is inside the repo. Active and stale memories (matching the
   existing query helpers' `status IN ('active','stale')` filter); stale ones are explicitly
   labeled in the rendered line, never silently mixed in. Obsolete memories are excluded.
4. **Lexical lane** (only when the symbol lane is empty): top-3 `search::lexical` hits with
   FTS/BM25 ranking only — explicitly no query embedding, no model load.
5. **Render** compact markdown: memories first (the unique signal), then symbols, then
   lexical hits; hard cap ~1,500 chars, truncating whole items (never mid-item). Each
   memory line carries kind + title + memory status (active/stale) and ends with a uniform
   `(rag-rat: memory_search)` dig-deeper pointer (symbol-bound and FTS memories are merged
   and deduped before render, so the pointer is lane-agnostic); the symbol section ends with
   an `(rag-rat: impact_surface <name> before editing)` pointer.
6. Empty after dedupe/caps → no output at all (the hook prints nothing).

## Bash command parsing

Conservative, table-tested tokenizer in the hook client:

- Shell-words split; scan pipeline/`&&`/`;` segments; skip env-var prefixes (`FOO=bar`) and
  a leading `cd … &&` segment.
- A segment matches when its command word (basename) is `grep`, `rg`, or `ag`.
- Pattern = first value of `-e`/`--regexp(=…)` if present, else the first positional
  argument after flag parsing (skipping flag values for the common arg-taking flags:
  `-A/-B/-C/-m/-g/--glob/--type/-t/--include/--exclude`). Second positional, if present,
  is `search_path`.
- Anything ambiguous (subshells, backticks, `xargs`, `find -exec`, no extractable
  pattern) → no-op. False negatives are fine; false positives are not.

## Install / uninstall / status

`hooks` subcommand grows a `--claude` mode (default remains git hooks; `--claude` switches
target):

- `rag-rat hooks install --claude` — merge into `<repo>/.claude/settings.json` (create if
  absent) two `PreToolUse` entries: matcher `Grep` and matcher `Bash`, both running
  `rag-rat claude-hook` with `"timeout": 10`. Merging is additive and marker-aware: entries
  are recognized as ours by the command string; existing unrelated hooks are preserved;
  re-install is idempotent.
- `rag-rat hooks install --claude --global` — same, into `~/.claude/settings.json`. The
  repo-resolution no-op (hook client flow, step 2) makes this safe everywhere.
- `rag-rat hooks uninstall --claude [--global]` — remove only our entries; preserve others;
  remove empty containers.
- `rag-rat hooks status --claude [--global]` — report presence/absence per matcher, like
  git-hook status today.
- JSON edits via `serde_json` preserving unknown fields (read → modify → write,
  pretty-printed 2-space like Claude Code writes it).

## Multi-session behavior

- N sessions → N `rag-rat mcp` processes → one socket-election winner per worktree; all
  sessions' hooks reach it; answers come from the shared DB, so which process won is
  irrelevant.
- Dedupe is keyed by the *hook input's* `session_id`, so sessions never suppress each
  other's context.
- Owner exit → surviving process wins the election retry and re-binds (watcher-identical
  pattern); during the gap, hook clients time out fast and use the stateless fallback.
- Multiple worktrees are independent: per-worktree socket path + election; the hook
  resolves its worktree from `cwd`.

## Error posture

The hook can only ever degrade to silence. No `deny`, no `ask`, no exit 2, no blocking.
Specific cases: no `rag-rat.toml` → silence; DB missing/locked/read-only-open fails →
silence; socket dead → fallback; fallback errors → silence; stdin not valid hook JSON →
silence. `RAG_RAT_HOOK_DEBUG=1` turns on stderr diagnostics (debug-log-only per the hook
contract, never model-visible).

## Testing

- **Unit (`rag-rat-cli` / hook client)**: bash-parser table (grep/rg/ag forms, `-e`,
  pipelines, env prefixes, ambiguous cases → None); settings.json merge/unmerge
  idempotence with foreign entries present.
- **Unit (`rag-rat-core::query::grep_augment`)**: payload composition against a seeded
  index — identifier vs non-identifier patterns, memory-first ordering, char cap, dedupe
  filter, empty-payload behavior.
- **Integration (`rag-rat-mcp`)**: spawn listener on a temp socket + temp DB; drive
  `claude-hook` end-to-end over stdin/stdout; assert `additionalContext` JSON shape;
  second identical request → `context: null` (dedupe); kill listener → same invocation
  succeeds via fallback (no dedupe).
- **Election**: two listeners race for the lock — exactly one binds; kill the winner —
  the loser takes over and unlinks the stale socket (mirrors existing watcher tests).
- **Hot-upgrade**: extend `mcp_hot_upgrade.rs` to assert the socket answers after handoff.
- **No-index no-op**: run the hook in a temp dir without `rag-rat.toml` → empty stdout,
  exit 0.

## Rejected alternatives

- **Direct-SQLite-only hook (approach A)** — works, but loses warm caches, and per-session
  dedupe would need on-disk state files with their own GC; the socket gives in-memory
  dedupe for free. Kept as the fallback path, which buys A's reliability anyway.
- **Reusing the watcher election for the socket** — would need core→mcp callback plumbing
  or moving the listener into core; a second lock file is two lines and keeps layering
  clean.
- **TCP localhost listener** — no port-collision story, worse hygiene than a
  filesystem-scoped UDS; Unix-only is already the posture of fleet/hot-upgrade.
- **Denying the grep and steering to MCP tools** — gating a critical operation on our
  availability; explicitly out (augment, never block).
- **Embedding-powered fallback lane** — ONNX session load (~0.5–2 s) per grep in the
  fallback path; FTS-only keeps the latency story honest.

## Non-goals

- Other agents' hook formats (Codex/Gemini/Zed adapters) — the socket protocol is
  agent-neutral, so adapters can come later without redesign.
- Augmenting `Glob`, `Read`, or MCP tool calls.
- Any write to the index from the hook path (read-only by construction).
- Windows support for the socket path (fallback covers it).
