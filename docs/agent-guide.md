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

Background the wrapped job with an events file, then follow the file:

```bash
watcher-s1 --silence 10m --timeout 2h --events /tmp/job.events -- nix build .#foo &
watcher-s1 follow /tmp/job.events
```

`follow` prints one line per event (`state reason severity exit s1 tail`) and
exits 0 when the run's final event arrives. The job's own exit code is what the
background task reports; read the final event for the verdict.

- Reusing an events file? Start the follow with `watcher-s1 follow --new FILE`
  so it ignores earlier runs. Without `--new` it reads from the start.
- `follow` waits for a file that does not exist yet, locks onto the first run
  it sees, and indents nested watchers. A nested watcher finishing does not end it.

## Flags worth picking

- `--silence DUR`: quiet time before `stalled` (default `300s`; `0` disables).
- `--timeout DUR`: hard limit, TERM then KILL for the whole process group.
- `--on-prompt cancel`: cancel an unanswered prompt instead of only reporting
  it (`--prompt-cancel-after DUR` sets the wait).
- `--events FILE`: where events go. Without it they land on stderr.
- `--pipe`: plain pipes instead of a PTY.
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
| `done` | `exit` | Final. Exit 0 and nothing flags it. |

A run ends with exactly one final event (`exit` is set). The `evidence_tail`
field carries the last lines of output; severity is `info`, `warn` or `error`.

## Stop a run

Send SIGTERM (or SIGINT) to the watcher-s1 process: it forwards the signal to
the whole process group, then KILLs after `--kill-grace`. The final event
records the signal. Do not kill only the child.

## Report problems

If watcher-s1 misled you (missed a stall, false alarm, confusing output, wrong
docs), open an issue on gerchowl/watcher-s1 with the `agent-feedback` label:

```bash
gh issue create --repo gerchowl/watcher-s1 --label agent-feedback --title "..." --body "..."
```

Include the command line, the events file, and what you expected.
