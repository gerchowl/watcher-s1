---
type: issue
state: open
created: 2026-11-02T08:25:58Z
updated: 2026-11-02T08:25:58Z
author: gerchowl
author_url: https://github.com/gerchowl
url: https://github.com/gerchowl/watcher-s1/issues/39
comments: 0
labels: agent-feedback
assignees: none
milestone: none
projects: none
parent: none
children: none
synced: 2026-11-02T09:12:19.445Z
---

# [Issue 39]: ["System One unavailable, failing open … timeout" during a CI watch on a loaded host](https://github.com/gerchowl/watcher-s1/issues/39)

Reported by a glint gateware delegate (2026-11-02, vm-dev, load ~25-56): while watching `gh pr checks 112 --watch` under watcher-s1, the log showed 'System One unavailable, failing open … timeout'. The watch itself completed and reported the right exit code. Expected: either no S1 dependency for a plain exit-code watch, or a clear one-line note that classification is degraded. Exact command/flags not captured by the reporter; next occurrence will include them.
