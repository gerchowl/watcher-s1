---
type: issue
state: open
created: 2026-10-08T18:46:26Z
updated: 2026-10-08T18:46:26Z
author: gerchowl
author_url: https://github.com/gerchowl
url: https://github.com/gerchowl/watcher-s1/issues/37
comments: 0
labels: bug, agent-feedback
assignees: none
milestone: Supervision correctness
projects: none
parent: none
children: none
synced: 2026-10-09T08:48:28.899Z
---

# [Issue 37]: [Expected negative gate fixture classified as masked_failure despite asserted rejection](https://github.com/gerchowl/watcher-s1/issues/37)

Observed installed watcher-s1 0.2.0 during nerdmachines/scryd#200 gate provenance validation on 2026-10-08. A shell deliberately runs a known-bad no-fake-impl fixture, asserts its exit is 1, prints confirmation, and exits 0. Watcher emitted state=failing, reason=masked_failure despite the expected-negative assertion and successful wrapper exit.

Evidence: run sage:54209:1791479307731, event 2026-10-08T17:08:38.344Z, exit.code=0; final log line: wrapped binary rejected known-bad fixture with exit 1. Scores: failing0.6776, clean_done0.5061, fused0.912148547541638. Local raw record: ~/.local/state/goal_s1/events/s1-soldevkitb1.jsonl; provenance command/log in builds/soldevkitb1.

Reproduction shape: run the packaged gate on a good fixture, then `if gate bad.rs; then exit 1; else status=$?; test "$status" = 1; fi`, followed by explicit expected-rejection confirmation. The bad fixture contains `fn f() { todo!() }`.

This is a classification false positive, not evidence that genuine masked failures should be suppressed. Add expected-negative cases alongside truly masked failures to calibration/regression coverage; retain detection of pipelines or shells that accidentally swallow unexpected failures. Determine whether an explicit expected-test-result contract is needed rather than blanket exit0 trust. No model/threshold change was made during this review.

Related consumer PR: https://github.com/nerdmachines/scryd/pull/200

https://claude.ai/code/session_011Yucgui9ZPs7zsSDWJ1RXU

