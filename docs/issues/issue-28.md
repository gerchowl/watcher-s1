---
type: issue
state: open
created: 2026-10-07T13:27:03Z
updated: 2026-10-07T14:24:15Z
author: gerchowl
author_url: https://github.com/gerchowl
url: https://github.com/gerchowl/watcher-s1/issues/28
comments: 1
labels: none
assignees: none
milestone: none
projects: none
parent: none
children: none
synced: 2026-10-07T15:05:49.236Z
---

# [Issue 28]: [tests/wrap.rs: a_grandchild_writing_forever_does_not_keep_us_alive can spin for minutes under heavy CPU contention](https://github.com/gerchowl/watcher-s1/issues/28)

Observed twice during #26 work while two cargo builds/test runs competed for the CPU: `a_grandchild_writing_forever_does_not_keep_us_alive` ran for minutes until killed, and `death_by_signal_is_re_raised` failed once. Both pass 10/10 in isolation, and the full suite is green when the machine is otherwise idle.

Worth confirming whether it is only a test-timing issue or a real drain-loop liveness problem under load (the drain cap is meant to bound exactly this case: a descendant writing forever after the leader exits). A repro under `stress-ng --cpu N` plus a hard per-test timeout would settle it.
---

# [Comment #1]() by [gerchowl]()

_Posted on October 7, 2026 at 02:24 PM_

Root cause found: not just test timing. The supervisor's output loop read without a per-pass limit while data kept arriving, so a fast writer (`yes &`) starved the exit check, the timers, `--timeout` and the control socket (the drain cap only applies after the leader's exit is noticed). Fixed in #29 (d4793c5): at most 16 reads per output per pass.

