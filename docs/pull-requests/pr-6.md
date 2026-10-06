---
type: pull_request
state: closed (merged)
branch: feat/question-set-v2 → main
created: 2026-10-05T21:31:09Z
updated: 2026-10-05T22:31:55Z
author: gerchowl
author_url: https://github.com/gerchowl
url: https://github.com/gerchowl/watcher-s1/pull/6
comments: 0
labels: none
assignees: none
milestone: none
projects: none
merged: 2026-10-05T22:31:52Z
synced: 2026-10-06T15:23:40.716Z
---

# [PR 6](https://github.com/gerchowl/watcher-s1/pull/6) feat: adopt the measured question set (logistic score, threshold per surface)

Item 1/2a: the question spike, and its adoption. Stacked on #3; GitHub retargets it to `main` when #3 merges.

**Spike** (`docs/eval/questions-spike.md`, harness in `eval/`):
- 1,474 real Bash calls from this host's transcripts, labelled by exit code (574 failures, 900 successes, piped commands excluded).
- Two views: the wrapper's 4 KB tail, and a `| tail -20` view for the judge.
- Ten candidate questions per request; logistic fit on a session-grouped dev split, measured once on held-out sessions.

| score | AUC | @0.5 P/R/FPR | @0.8 P/R/FPR |
|---|---|---|---|
| **logistic, 10 questions (test)** | **0.951** | 0.93/0.81/4.0 % | 0.95/0.71/2.2 % |
| `outcome` choice alone | 0.932 | 0.92/0.81/4.3 % | 0.98/0.47/0.7 % |
| previous default (fused) | 0.882 | 0.87/0.63/5.8 % | 0.98/**0.18**/0.2 % |

**Adopted:**
- logistic score, with `threshold = { wrap = 0.5, judge = 0.8 }` (the per-surface choice from the owner);
- the wrapper's exit-0 regex gate is dropped, since it caught 34 % of failures;
- question files gain `kind = "logistic"`, choice-sum features and per-surface thresholds; `kind = "mean"` still works.

**Live Kev:** a failing `cargo test` summary now scores 0.89 (was 0.45) and the hook flags it; a clean one scores 0.13.

**Limitations:** one host, exit code as the label, and a simulated masked-pipe view. Details in the doc.


---
---

## Commits

### Commit 1: [69757b2](https://github.com/gerchowl/watcher-s1/commit/69757b21c62e254ae51e4277a1503b0f529ec042) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 09:30 PM
feat: adopt the measured question set (logistic score, threshold per surface), 1269 files modified
