---
type: issue
state: closed
created: 2026-10-07T10:33:32Z
updated: 2026-10-07T16:05:05Z
author: gerchowl
author_url: https://github.com/gerchowl
url: https://github.com/gerchowl/watcher-s1/issues/20
comments: 2
labels: none
assignees: none
milestone: none
projects: none
parent: none
children: none
synced: 2026-10-08T08:43:13.616Z
---

# [Issue 20]: [feat: watcher-s1 mcp — MCP server (start/wait/status/stop runs, guide resource, Channels push)](https://github.com/gerchowl/watcher-s1/issues/20)

## Motivation

An agent that drives watcher-s1 through the shell has to manage a background process, an events-file path and a `follow` loop (#19). An MCP server makes all of that tool calls. With Claude Code **Channels**, it can also **push** events into the session, so the agent hears `stalled` or the final exit without polling.

The CLI stays the primitive: hooks, fleet jobs and plain shells can't speak MCP. This is a thin layer on #18 (`--heartbeat`) and #19 (`guide`, `follow`).

## Proposal: `watcher-s1 mcp` (stdio)

**Tools**
- `watch_start {cmd: [..], cwd?, silence?, timeout?, heartbeat?, pipe?, s1?}` runs a fully wrapped `watcher-s1 --events <state>/runs/<id>.jsonl … -- cmd`.
  - The job is **detached** (`setsid`), so it survives the server or session ending.
  - Child output goes to `<state>/runs/<id>.log`.
  - Returns `run_id`, the events path and the log path.
- `watch_wait {run_id, until: "next"|"final", timeout_s}` blocks until the next event (or the final one) and returns the event(s). Over 2 min, Claude Code backgrounds the call automatically.
- `watch_status {run_id}`: last event, elapsed time, alive or not.
- `watch_stop {run_id, signal?}`: TERM, then KILL after the grace period, through the watcher, so the final event stays truthful.
- `watch_list`: runs in the state dir (`$XDG_STATE_HOME/watcher-s1`), newest first.

**Resource:** `watcher-s1://guide`, the same compiled-in guide as `watcher-s1 guide`.

**Push (opt-in `--channel`):**
- The server declares `experimental: {"claude/channel": {}}`.
- For runs it started, it sends one `notifications/claude/channel` per **edge** event (`stalled`, `waiting_on_input`, `failing`, final). Heartbeats are excluded unless asked for.
- Content is the compact `follow` line; `meta` carries `run_id`, `state` and `reason`.
- Caveats: research preview (Claude Code ≥ 2.1.80, org opt-in on Team/Enterprise). Events arrive on the next turn and don't wake an idle session, so `watch_wait` remains the blocking path.

## Implementation choice

- **Hand-rolled (recommended):** synchronous JSON-RPC 2.0 over stdio. The surface is `initialize`, `tools/list`, `tools/call`, `resources/list`, `resources/read` and the notification, and the binary stays dependency-light and sync.
- **Alternative:** the official `rmcp` crate behind a cargo feature. It tracks the spec for us but pulls in tokio.
- **Tests either way:** a scripted JSON-RPC session (initialize, then start, wait, final) in `tests/`, plus one manual run under Claude Code with `--channel`.

## Pitfalls

- Runs must outlive the server; `watch_wait` and `watch_status` work on runs started by an earlier server process (the state dir is the source of truth).
- stdout is the protocol, so nothing else may print there. Diagnostics go to stderr.
- Bounded growth of the state dir: prune runs older than N days on start.

## Acceptance

- [ ] `watcher-s1 mcp` passes a scripted protocol test: initialize, tools/list, `watch_start` → `watch_wait final` returns the final event with the right exit.
- [ ] A run survives the MCP server being killed, and a new server instance can `watch_wait` on it.
- [ ] `--channel`: an edge event produces a `notifications/claude/channel` line (protocol test). Manual check in Claude Code documented.
- [ ] `docs/agent-guide.md` gains an MCP section; the drift test covers the `mcp` flags.

Related: #18, #19, gerchowl/claude-config#29.

---

# [Comment #1]() by [gerchowl]()

_Posted on October 7, 2026 at 11:15 AM_

**Decided:**
- **Build:** the official `rmcp` crate, behind an `mcp` cargo feature that is **on by default**, so release binaries and the nix package include it; `--no-default-features` builds without it. Version pinned via `Cargo.lock` and tracked by Renovate's cargo manager, like every other dependency.
- **Ships in 0.2.0** together with #18 and #19.
- **Order:** implementation starts once #21 (heartbeat) and #22 (guide/follow) are merged, because it reuses `follow`'s event parsing and the compiled-in guide.

---

# [Comment #2]() by [gerchowl]()

_Posted on October 7, 2026 at 04:05 PM_

Shipped in v0.2.0 (#25, hardened in #29).

