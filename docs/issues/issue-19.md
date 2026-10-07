---
type: issue
state: closed
created: 2026-10-07T10:27:59Z
updated: 2026-10-07T12:08:24Z
author: gerchowl
author_url: https://github.com/gerchowl
url: https://github.com/gerchowl/watcher-s1/issues/19
comments: 1
labels: none
assignees: none
milestone: none
projects: none
parent: none
children: none
synced: 2026-10-07T15:05:50.496Z
---

# [Issue 19]: [feat: watcher-s1 guide + follow — the binary is the single source for agent usage](https://github.com/gerchowl/watcher-s1/issues/19)

## Motivation

How an agent should use watcher-s1 (flags to pick, events to expect, how to react) is now written in several places: the README, the `watch-job` skill (gerchowl/claude-config#29), and soon whatever else teaches agents. Each copy drifts the moment a flag or event changes (e.g. `--heartbeat`, #18). The installed binary is the only thing guaranteed to match itself.

## Proposal

1. **`watcher-s1 guide`** — prints the agent guide (Markdown) for THIS version: when to use it, the start command, the flags worth picking, the event → reaction table, how to stop a run, where to report problems (`agent-feedback` label). Compiled in (`include_str!` of `docs/agent-guide.md`), so every host's guide matches its binary — no lock bump, no lag.
2. **`watcher-s1 follow EVENTS_FILE`** — prints one compact line per event (`state reason severity exit s1 tail`) and exits after the final event (non-null `exit`); waits for the file to appear. Replaces the shell helper `watch-events` in the skill (poll loop + jq). Exit code: 0 after a final event, 2 on usage error.
3. **Drift test** (cargo test, so CI and the pre-commit hook both run it):
   - every `--flag` mentioned in `docs/agent-guide.md` and README is a real clap flag (walk `Cli::command()`),
   - every `state`/`reason` value in `event.schema.json` appears in the guide's event table, and vice versa.

After this, the claude-config skill shrinks to a pointer ("run `watcher-s1 guide`, use `watcher-s1 follow`"), and claude-config's `scripts/check-ssot.sh` moves the owner from the skill to the binary.

## Pitfalls

- The guide must stay agent-sized (≈100 lines), not a second README; the README can link it.
- `follow` must handle the events file not existing yet (the background job may not have started) and a partial last line.
- Old binaries lack `guide`: the skill should say "needs ≥ 0.2.0" and fall back to the README link.

## Acceptance

- [ ] `watcher-s1 guide` prints the guide; `watcher-s1 follow` exits after the final event.
- [ ] The drift test fails when a flag in the guide is renamed or a schema state is missing from the table.
- [ ] Released (0.2.0), then the claude-config skill reduced to a pointer.

Related: #18 (`--heartbeat`), gerchowl/claude-config#29.

---

# [Comment #1]() by [gerchowl]()

_Posted on October 7, 2026 at 10:31 AM_

## Review: approve, with these decisions settled

1. **`guide`:** `include_str!("../docs/agent-guide.md")`. This needs `/docs/agent-guide.md` in `Cargo.toml` `include` and in `flake.nix` `extraSrcFiles`; without them crane's source filter drops it and the nix build breaks. Keep it to ≈100 lines and link it from the README.
2. **`follow`:**
   - **Nested watchers and reused files.** Nested watchers can append to the same events file, and a file can be reused across runs. So `follow` **locks onto the first run it sees** (its `run_id`), prints every event (nested ones indented by their `caused_by` depth), and exits **only on that run's final event** (non-null `exit`). An inner `nix build` watcher finishing must not end it.
   - **`--new`:** seek to the end before following, ignoring prior content. Use it when reusing a file; without `--new` it reads from the start.
   - **Waiting:** if the file doesn't exist yet, wait for it. A partial last line is buffered until its newline. A malformed line triggers one stderr warning and is skipped.
   - **Exit codes:** 0 after the final event, 2 on usage error. Line format: `state reason severity exit s1 tail`, with the tail being the last evidence line, truncated.
3. **Drift test:** a Rust test against `lib`'s `Cli::command()`, covering top-level and subcommand flags (`judge`, `config`, `follow`, `guide`).
   - **Flag scan:** every backticked `--flag` and every `--flag` in a code-block line containing `watcher-s1` (guide and README) must be a real flag. Foreign flags (`cargo --locked`, `gh --ref`, …) go in an explicit allowlist in the test, so the intent is visible.
   - **Event table:** every `state` and `reason` enum value in `event.schema.json` appears in the guide's event table, and every value in the table exists in the schema.
4. **Order:** this lands after #18, and the drift test then forces the `heartbeat` row into the table. Release 0.2.0 after both. The claude-config skill reduction is claude-config's change: once 0.2.0 is out, a comment on gerchowl/claude-config#29 hands it over.


