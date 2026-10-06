# Question-set spike: which System One score flags a failure?

Follow-up to [g-fleet#244](https://github.com/gerchowl/g-fleet/issues/244). The first
built-in set (`failing` + `clean_done`, fused, flag at 0.8) was taken from the
#244 measurement, and live checks showed it missed failing test-runner
summaries. This spike asks: **which questions, and which score, flag a real
failure from the output tail alone?**

Harness in [`eval/`](../../eval): `extract.py` (labels), `query.py`
(System One calls), `score.py` (metrics), `questions_candidates.toml`. The
corpus is raw command output, so it stays local and is never committed.

## Method

- **Corpus:** every Bash call in one host's Claude Code transcripts (199
  sessions, 63,743 calls), each `tool_use` paired with its `tool_result`.
- **Labels:** the real exit code (`fail` = the harness's `Exit code N`, N > 0;
  `ok` = a successful result). The `Exit code` prefix is stripped. Excluded:
  - background launches, timeouts, interrupts;
  - outputs under 20 characters (nothing to judge);
  - **piped commands**, whose status is a filter's (as in #244).
- **Sample:** all 574 clean failures, plus 900 random clean successes (seed 244).
- **Two views per command**, exit status hidden, in the state shape the
  binary sends:
  - `wrap4k`: the last 4 KB, ANSI-stripped, what the wrapper sends;
  - `tail20`: the last 20 lines, what the PostToolUse judge sees after
    `| tail -20`, with the true label kept. The masked-pipe condition is
    simulated on ground-truth outcomes, because rerun-based masked labels
    (#244's rule R1) are too rare in these transcripts (0 found).
- **Ten candidate questions** (`eval/questions_candidates.toml`), all sent in
  one request per state (2,948 sequential calls; p50 latency 468 ms):
  - the two measured ones;
  - goal-s1's `red`;
  - six new `noul` phrasings;
  - an `outcome` choice (success / failure / partial / info).
- **Logistic combination:** fitted on a session-grouped dev split (~60 %) and
  measured **once** on the held-out sessions. The single-question rows use all
  rows (they have nothing to fit).

## Results: wrapper view (4 KB tail)

| score | AUC | recall @ precision 0.90 | @0.5 P / R / FPR | @0.8 P / R / FPR |
|---|---|---|---|---|
| **logistic, all 10 (test split)** | **0.951** | **0.86** | 0.93 / 0.81 / 4.0 % | 0.95 / 0.71 / 2.2 % |
| `outcome`: failure + partial | 0.932 | 0.82 | 0.92 / 0.81 / 4.3 % | 0.98 / 0.47 / 0.7 % |
| `ends_with_error` | 0.925 | 0.75 | 0.93 / 0.70 / 3.1 % | 0.97 / 0.53 / 0.9 % |
| `red` (goal-s1) | 0.925 | 0.77 | 0.92 / 0.72 / 4.0 % | 0.98 / 0.49 / 0.7 % |
| `exit_status` | 0.922 | 0.76 | 0.93 / 0.70 / 3.4 % | 0.98 / 0.52 / 0.8 % |
| `any_failure` | 0.909 | 0.72 | 0.83 / 0.78 / 10.2 % | 0.92 / 0.58 / 3.2 % |
| **fused (previous default)** | 0.882 | 0.58 | 0.87 / 0.63 / 5.8 % | 0.98 / **0.18** / 0.2 % |
| `failing` | 0.881 | 0.64 | 0.90 / 0.65 / 4.8 % | 0.97 / 0.16 / 0.3 % |
| `tests_failed` | 0.842 | 0.00 | 0.87 / 0.64 / 6.2 % | 0.55 / 0.03 / 1.6 % |

The `tail20` view gives the same picture: logistic AUC 0.950, previous
default 0.868. `python3 eval/score.py <answers.jsonl>` reproduces every number.

**Findings**

- The previous default flagged only **18 %** of real failures at its
  threshold. `failing` and `clean_done` are the two weakest predictors of the
  ten: `clean_done` reads "the run reached its summary" as a clean end.
- One choice question (`outcome`) beats every `noul` phrasing. Combining all
  ten adds roughly 4 points of recall at every precision.
- The wrapper's old **regex pre-filter** (it only asked System One about an
  exit 0 when an error line matched) fired on only **34 %** of failures. It
  is removed: with System One configured, every exit 0 that produced output
  is judged once, at exit.
- On live Kev after adoption, a failing `cargo test` summary scores 0.89
  (previously 0.45), and a clean one 0.13.

## Decision: logistic score, threshold per surface

`questions/builtin.toml` now holds the ten questions and the dev-fitted
weights, with `threshold = { wrap = 0.5, judge = 0.8 }`.

Failures are **rare** where these run, so the false-positive rate dominates
what a flag is worth. The precision to expect at a given failure base rate:

| surface, threshold | recall | FPR | precision at 5 % failures | at 10 % | at 39 % (this eval) |
|---|---|---|---|---|---|
| wrapper, 0.5 (4 KB view) | 0.81 | 4.0 % | 0.51 | 0.69 | 0.93 |
| judge, 0.8 (20-line view) | 0.71 | 2.2 % | 0.63 | 0.78 | 0.95 |

Both rows are each view's own test split (732 commands), scored with the
weights fitted on the 4 KB view's dev split. That the judge row equals the
4 KB view's 0.8 numbers above is a measured coincidence, not a copy;
`python3 eval/score.py` → `deployment_report` printed:

```text
wrap4k test n=732 @0.5: precision 0.93 recall 0.81 FPR 0.040 AUC 0.951
tail20 test n=732 @0.8: precision 0.95 recall 0.71 FPR 0.022 AUC 0.950
```

- **Wrapper at 0.5.** It judges rare events (a run's exit, a silence
  threshold), its output goes to a gateway or human, and missing a failure
  is the costlier error.
- **PostToolUse judge at 0.8.** It fires on every piped exit-0 Bash call, so
  a false flag interrupts an agent mid-task; the stricter threshold keeps
  about 2 in 3 flags real at a 5 % base rate.

## Limitations

- One host and one population: agent Bash calls, mostly cargo, nix, git and
  gh. Long-running fleet jobs (the wrapper's main use) are under-represented.
- The label is the exit code. Non-zero exits that are not failures (`grep`
  with no match, `diff`, `test`) count as failures, which caps the
  achievable recall.
- The masked-pipe view is simulated (the last 20 lines of real outputs), not
  observed. Re-measure on observed masked pipes once the hook has logged some.
- Changing any question's wording changes its answers. Re-run `eval/` and
  refit the weights before editing `questions/builtin.toml`.
