---
type: issue
state: open
created: 2026-10-08T14:27:33Z
updated: 2026-10-08T14:27:33Z
author: gerchowl
author_url: https://github.com/gerchowl
url: https://github.com/gerchowl/watcher-s1/issues/36
comments: 0
labels: none
assignees: none
milestone: none
projects: none
parent: none
children: none
synced: 2026-10-09T08:48:29.213Z
---

# [Issue 36]: [System One switches from Kev-4B to Clef (g-fleet#330): re-run the question eval](https://github.com/gerchowl/watcher-s1/issues/36)

## Heads-up: Kev-4B on sage is being replaced by Clef (gerchowl/g-fleet#330)

The fleet's System One endpoint changes model. **Nothing has switched yet.** This issue is so this repo is ready before it does.

| Endpoint | Today | After the cutover |
|---|---|---|
| `http://sage.tail22bd7c.ts.net:8023/v1/systemone` (`SYSTEMONE_URL`) | Kev-4B | **`clef-flash`** (Cloudflare, 9B): text only, fast |
| `http://sage.tail22bd7c.ts.net:8024/v1/systemone` (new, `SYSTEMONE_URL_FULL`) | none | **`clef`** (Cloudflare, 27B): text, image, video* |

\* Video is still to be verified on the chosen engine (g-fleet#330).

**Same URL, same API** (`/v1/systemone`, `state` + typed `questions`: `noul`/`choice`/`score`), and no `model` field needed: each port serves one model. Text-only callers need no code change to keep working.

### What can still break, and needs checking here

- **Calibration:** Clef's probabilities are not Kev's. Any threshold, margin or AUC-tuned cut-off chosen against Kev must be re-checked against clef-flash.
- **Latency:** clef-flash is reported ~39 ms on an H200. On sage (MLX/Metal) it's unmeasured, and the 27B is several times slower. Check every hard timeout.
- **Response shape:** Clef is described as TypeSafe/Jev-compatible, like Kev, but fields beyond `answers` (e.g. `usage`, `model` name `kev-latest`, `x-typesafe-request-id`, truncation flags, Kev's `/permute` and `/separate` extras) may differ. Anything parsing beyond `answers` needs a check.
- **State limits:** Kev refused over-long states with a 422 (or truncated them with `KEV_TRUNCATE_STATES`). Clef's limit and behaviour differ (hosted: 65,536 tokens, truncates).
- **Names:** user-facing strings, docs and log fields that say "Kev" become wrong; rename them to "System One" or the model name, where it isn't already neutral.

### In this repo

- `src/config.rs` already reads `SYSTEMONE_URL` (and a list), so the URL keeps working. Consider a second key or env (`SYSTEMONE_URL_FULL`) for evidence with images or video, e.g. pane screenshots.
- `questions/builtin.toml`: the question set and its event-time AUC (`.92` vs regex `.83`, `docs/eval/questions-spike.md`) were measured on Kev. Re-run that eval on clef-flash before relying on the verdicts.
- Known Kev failure mode (#35: healthy quiet jobs flagged): re-check it on Clef, since it may get better or worse.
- `tests/kev.rs`, `tests/s1.rs`: mock and live expectations; check response-shape assumptions beyond `answers`.
- Docs and CHANGELOG mention Kev by name.

### Acceptance
- [ ] Question-set eval re-run on clef-flash; results in docs/eval.
- [ ] Live tests pass against :8023 after the cutover.
- [ ] Decision documented on whether and how the full (vision) endpoint is used.

