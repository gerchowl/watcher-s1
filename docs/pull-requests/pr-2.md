---
type: pull_request
state: closed (merged)
branch: feat/posttooluse-judge → main
created: 2026-10-05T14:13:54Z
updated: 2026-10-05T14:20:21Z
author: gerchowl
author_url: https://github.com/gerchowl
url: https://github.com/gerchowl/watcher-s1/pull/2
comments: 0
labels: none
assignees: none
milestone: none
projects: none
merged: 2026-10-05T14:20:19Z
synced: 2026-10-06T15:23:45.615Z
---

# [PR 2](https://github.com/gerchowl/watcher-s1/pull/2) feat: judge --posttooluse, a Claude Code hook for masked pipes

Step 2 of gerchowl/g-fleet#244: `watcher-s1 judge --posttooluse`, a Claude Code PostToolUse hook for Bash that catches masked pipes.

## Behaviour
The hook contract was checked against the current docs (code.claude.com/docs/en/hooks). PostToolUse fires **only for successful calls**; a non-zero exit goes to `PostToolUseFailure`. So every input is an exit 0, and `tool_response` is `{stdout, stderr, interrupted, isImage}`. The hook speaks through `hookSpecificOutput.additionalContext`, which reaches the model next to the tool result.

It acts only when all of these hold:
- `tool_name == "Bash"`, not `run_in_background`, not `interrupted`
- the command pipes into a filter as the last stage of some statement (`tail`, `head`, `grep`, `rg`, `sed`, `awk`, `sort`, `tee`, `jq`, …). A small quote-, `$(…)`- and subshell-aware splitter finds this. Commands that mention `pipefail` are skipped.
- a System One endpoint is configured (same precedence as the wrapper)

It then judges the last 4 KB of stdout+stderr in one request. If fused ≥ threshold (0.8 by default) it prints the additionalContext: *"exit 0 came from the pipe; the output shows an unrecovered failure: <evidence> (System One fused score X >= 0.80)…"*. The evidence line is the last error-panel hit, else the last non-empty line. Otherwise it prints nothing.

**Budget:** a watchdog thread exits 0 at 2.9 s, whatever is hung (stdin, DNS, endpoint), and the System One call gets the remaining budget. It always exits 0 and is silent on stderr. Any failure (bad JSON, endpoint down, breaker open) fails open.

The README has the settings.json snippet (exec form, `"timeout": 5`). Wiring it into claude-config/g-fleet is left to those repos, as scoped.

## Tests
- Fake System One: flags a masked pipe with evidence; stays silent below threshold, unpiped, or with no endpoint; a hung endpoint returns inside 3.2 s with no output; garbage stdin is a silent exit 0.
- Unit tests for pipe detection (quotes, `||`, `|&`, `$(…)`, `pipefail`, absolute paths).
- Real Kev (opt-in, sequential), against sage: a compile failure piped through `tail` is flagged (fused 0.86); clean output and a failing *test summary* (fused ≈ 0.47) stay silent. The latter is the fused-score limitation reported on #1, carried over here unchanged because the threshold is the design's.



---
---

## Commits

### Commit 1: [92d9701](https://github.com/gerchowl/watcher-s1/commit/92d97015e47f4c10c13348ca158a5283e17007e5) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 01:43 PM
feat: add judge --posttooluse, a Claude Code hook for masked pipes, 295 files modified (src/cli.rs, src/judge.rs, src/lib.rs, src/main.rs)

### Commit 2: [97a4f65](https://github.com/gerchowl/watcher-s1/commit/97a4f658bc37825050263ceb22171f04accb9e00) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 01:43 PM
test: cover the PostToolUse judge against the fake and real System One, 149 files modified (tests/kev.rs, tests/s1.rs)

### Commit 3: [343d70f](https://github.com/gerchowl/watcher-s1/commit/343d70f10881868d40656af02f34a17695a4f4e6) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 01:43 PM
docs: document the PostToolUse hook and its settings.json snippet, 41 files modified (README.md)

### Commit 4: [8645ab7](https://github.com/gerchowl/watcher-s1/commit/8645ab7bbaa755b71db261dae2f8e96ac0e15ae2) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 02:13 PM
docs: list judge --posttooluse in the usage synopsis, 1 file modified (README.md)
