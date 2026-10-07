# watcher-s1: agent guide

Needs watcher-s1 >= 0.2.0 (`watcher-s1 --version`). Print this guide any time
with `watcher-s1 guide`.

## When to use it

Wrap any command that may run long or unattended (builds, test suites, deploys,
installs) and could hang on a prompt, go silent, or exit 0 while hiding a
failure. Skip it for quick commands you will wait on anyway.

watcher-s1 never changes the command's exit status and never answers a prompt.
It only reports, as JSON lines (one event per line) in a file you choose.

## Start a run

Use one fresh, unique events file per run, start the watcher in the
background, then follow the file:

```bash
EV="$(mktemp -d)/job-$(date +%s)-$RANDOM.events"
watcher-s1 --silence 10m --timeout 2h --events "$EV" -- nix build .#foo &
watcher-s1 follow --timeout 3h "$EV"
```

Background the watcher with the Bash tool's `run_in_background` in Claude
Code, or `&` in a plain shell. `follow` waits for the file to appear and reads
it from the start, so a job that is quick or quiet cannot slip past it, and no
earlier run can leak in. It prints one line per event (`state reason severity
exit s1 tail`) and exits 0 when the run's final event arrives. The job's own
exit code is what the background task reports; read the final event for the
verdict.

- `follow --timeout DUR`: give up if the final event has not arrived in time.
  It prints a one-line note to stderr and exits 3. Set it past the job's own
  `--timeout`.
- Exit codes of `follow`: 0 final event seen, 1 I/O error, 2 usage error,
  3 `--timeout` elapsed.
- `follow --new FILE` is only for attaching to a reused file while ignoring
  runs already in it. Start it BEFORE the job: it skips everything the file
  holds, so a final event written before it opens the file is missed and it
  waits forever (use `--timeout`).
- `follow` locks onto the first run it sees and indents nested watchers. Once
  locked, other runs' final events cannot end it (a quiet outer watcher that
  has emitted nothing yet means the first run seen may be an inner one). It
  copes with the file being truncated or replaced (rotated: the new file is
  read from the start, the old one is still read for late writes). A file
  reused in place is only noticed when its first 64 bytes change or it
  shrinks, so use a fresh file per run.
- `follow` reads regular files only (a FIFO is refused with exit 2). If its
  stdout reader stops, a write can block and `--timeout` cannot fire.

## Flags worth picking

- `--silence DUR`: quiet time before `stalled` (default `300s`; `0` disables).
- `--timeout DUR`: hard limit, TERM then KILL for the whole process group.
- `--on-prompt cancel`: cancel an unanswered prompt instead of only reporting
  it (`--prompt-cancel-after DUR` sets the wait).
- `--events FILE`: where events go. Without it they land on stderr.
- `--pipe`: plain pipes instead of a PTY.
- `--heartbeat DUR` (min `1s`, with `--events`): a periodic `heartbeat` event
  with `elapsed_ms` and `last_line`. `--heartbeat-s1` adds a System One call per
  tick, so costs one each.
- `--no-s1`: skip the System One judgement (tiers 0 and 1 still run).
- `--quiet`: no `watcher-s1 (log):` diagnostics on stderr.

## Events and how to react

| `state` | `reason` | React |
|---|---|---|
| `progressing` | `resumed` | Output resumed after a warning. Nothing to do. |
| `stalled` | `silence`, `blocked` | No output, or a process stuck in `D`/`U` state. Look at the tail; give it time, or stop it if the tail shows it is wedged. |
| `waiting_on_input` | `prompt` | A prompt nobody will answer. Stop the run and re-run non-interactively (`-y`, `--yes`, `CI=1`). |
| `failing` | `silence` | Silent and the output reads as a failure. Treat as failed; stop it. |
| `failing` | `masked_failure` | Final. Exit 0, but the output shows a failure. Do not trust the success. |
| `failing` | `exit`, `signal`, `timeout` | Final. Non-zero exit, killed by a signal, or killed by `--timeout`. Read the tail, fix, re-run. |
| `failing` | `prompt_cancelled` | Final. `--on-prompt cancel` ended an unanswered prompt. Re-run non-interactively. |
| any | `heartbeat` | Periodic liveness, state is the current one. No action unless the state is `stalled` or `waiting_on_input`. |
| `done` | `exit` | Final. Exit 0 and nothing flags it. |

A run ends with exactly one final event (`exit` is set); heartbeats are never final. The `evidence_tail`
field carries the last lines of output; severity is `info`, `warn` or `error`.

## Stop a run

Prefer `--timeout DUR` on the watcher: it sends TERM to the job's process
group and KILL after `--kill-grace`, so the bound is guaranteed.

To stop a run by hand, send SIGTERM (or SIGINT) to the watcher-s1 process. It
only forwards that signal to the job's process group; there is no escalation,
so a job that ignores TERM keeps running. To force-stop, send SIGKILL to the
job's process group, `kill -KILL -<pgid>` (`pgid` is in every event); the
watcher then reports the signal exit truthfully. Never SIGKILL the watcher
itself: it cannot forward anything and would orphan the job.

## Over MCP

If your client has the `watcher-s1` MCP server (`watcher-s1 mcp`), use its tools
instead of the shell recipe above. `watch_start {cmd: [argv], silence?,
timeout?, heartbeat?}` runs the job detached (it outlives the session) and
returns an `id`. `watch_wait {id, until: "final", timeout_s}` blocks until the
verdict (check `timed_out`, wait again if set); `until: "next"` returns events
you have not seen. `watch_status {id}`, `watch_list` and `watch_stop {id}`
cover the rest: stop sends TERM, then SIGKILL to the job's group after
`grace_s` (default 5). Events and reactions are the same as below, and a new
session can `watch_wait` on a run an earlier one started. With `--channel`,
edge events arrive on their own as `<channel>` messages carrying the same
`id` as `run_id`.

## Report problems

If watcher-s1 misled you (missed a stall, false alarm, confusing output, wrong
docs), open an issue on gerchowl/watcher-s1 with the `agent-feedback` label:

```bash
gh issue create --repo gerchowl/watcher-s1 --label agent-feedback --title "..." --body "..."
```

Include the command line, the events file, and what you expected.
