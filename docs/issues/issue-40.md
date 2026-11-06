---
type: issue
state: open
created: 2026-11-06T03:09:16Z
updated: 2026-11-06T03:09:16Z
author: gerchowl
author_url: https://github.com/gerchowl
url: https://github.com/gerchowl/watcher-s1/issues/40
comments: 0
labels: agent-feedback
assignees: none
milestone: none
projects: none
parent: none
children: none
synced: 2026-11-06T09:09:22.517Z
---

# [Issue 40]: [agents babysit long sweeps with 10-minute until-loops instead of one watcher-s1 run](https://github.com/gerchowl/watcher-s1/issues/40)

In 3 days, cx delegates made 767 poll or wait calls, totalling ~35 h of wall time. 249 of them lasted more than 60 s. Typical form: `timeout 595 bash -c 'until grep -q "exit" …sweepN.events; do sleep 20; done'`, or `watcher-s1 follow --timeout 9m` repeated 24 times per sweep. Each repeat re-reads the agent's whole context.
Why: a single blocking call can't outlast the tool timeout, so agents slice the wait.
Ask: make the documented path a single background `watcher-s1 run -- <cmd>` (or an attach to an already running job by pid or events file) that wakes the agent on exit, stall or prompt. Its guide should show this specific anti-pattern and its replacement. Evidence: a tool-usage analysis of the 91 cx agent transcripts from 2026-11-03 to 06 (15,256 real calls; report in the cx architect state, research/tool-usage-digest-20261106.md).
