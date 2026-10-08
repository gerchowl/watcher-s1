---
type: issue
state: open
created: 2026-10-07T23:59:48Z
updated: 2026-10-07T23:59:48Z
author: gerchowl
author_url: https://github.com/gerchowl
url: https://github.com/gerchowl/watcher-s1/issues/35
comments: 0
labels: agent-feedback
assignees: none
milestone: none
projects: none
parent: none
children: none
synced: 2026-10-08T08:43:12.478Z
---

# [Issue 35]: [guide: recipe defaults to mktemp /tmp, foreground follow, and 'failing → stop it' (Kev flags healthy quiet jobs)](https://github.com/gerchowl/watcher-s1/issues/35)

From an independent review of claude-config#33, where the watch-job skill shrank to a pointer to `watcher-s1 guide` (0.2.0). The guide lost three pieces of guidance the old skill had:
1. **`/tmp` default:** the recipe uses `mktemp -d`, which lands in `/tmp`. On hosts where `/tmp` is RAM (vm-dev), the events file and log should go under `${XDG_STATE_HOME:-~/.local/state}/watcher-s1/…`. Make that the recipe's default.
2. **Agent harness waiting:** `follow --timeout 3h` shown as a plain command gets killed by agent tool timeouts, e.g. the Claude Code Bash tool's 2 min default and 10 min maximum. Say to run both the watcher and `follow` as background tasks in agent harnesses.
3. **Verdict tone:** the guide says to treat `failing/silence` as failed and stop it. The old skill called it a hint, not a verdict, because Kev has flagged healthy quiet jobs with high scores. Consider 'read the log before stopping', plus a note never to answer credential prompts.

Also, `--log` passive mode is in `--help` but not in the guide.

claude-config now repeats points 1-3 locally (no flags) until the guide carries them.
