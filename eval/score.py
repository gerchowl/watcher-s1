#!/usr/bin/env python3
"""Score the candidate System One questions (eval/query.py output).

Usage: score.py ANSWERS.jsonl [--seed S]

Per view (wrap4k: the wrapper's 4 KB tail; tail20: a `| tail -20` view as
the PostToolUse judge sees it) and per candidate score:
  AUC, recall at precision >= 0.90 / 0.95, and at fixed thresholds the
  precision, recall and false-positive rate on clean exit-0 rows.
Fusions are fixed formulas, plus a logistic combination fitted on a
session-grouped dev split (60 %) and measured once on the test split.
Also reports the recall of the wrapper's own error-line pre-filter (an
exit 0 only reaches System One when it fires).
"""

import argparse
import hashlib
import json
import math
import re
import sys

NOUL = [
    "failing",
    "clean_done",
    "red",
    "any_failure",
    "exit_status",
    "tests_failed",
    "ends_with_error",
    "succeeded",
    "error_present",
]

# Mirror of src/detect.rs ERRORS (the wrapper's exit-0 pre-filter).
PANEL = [
    re.compile(p)
    for p in [
        r"(?m)^\s*error(\[E\d+\])?:",
        r"(?m)^\s*(fatal|FATAL)( error)?:",
        r"test result: FAILED",
        r"(?m)^(FAILED|FAIL)\b|\bFAILED\b",
        r"panicked at",
        r"Traceback \(most recent call last\)",
        r"(?m)^\s*[A-Za-z_.]*(Error|Exception):",
        r"npm ERR!",
        r"(?i)segmentation fault|core dumped|bus error",
        r"(?i)command not found",
        r"error: builder for .* failed",
        r"(?i)\b(build|compilation|tests?) failed\b",
        r"(?i)exit(ed)? (with )?(code|status) [1-9]",
        r"(?m)^\s*E\s{2,}",
    ]
]


def panel_hits(text):
    return any(p.search(line) for line in text.split("\n") for p in PANEL)


def features(a):
    f = {q: a[q]["noul"] for q in NOUL if q in a and "noul" in a[q]}
    pr = (a.get("outcome") or {}).get("probabilities") or {}
    f["outcome_fail"] = pr.get("failure", 0) + pr.get("partial", 0)
    f["succeeded_inv"] = 1 - f["succeeded"]
    f["clean_done_inv"] = 1 - f["clean_done"]
    return f


FUSIONS = {
    "fused(current)": lambda f: (f["failing"] + f["clean_done_inv"]) / 2,
    "max(failing,any_failure)": lambda f: max(f["failing"], f["any_failure"]),
    "max(failing,tests_failed)": lambda f: max(f["failing"], f["tests_failed"]),
    "mean(red,any_failure,exit_status)": lambda f: (
        (f["red"] + f["any_failure"] + f["exit_status"]) / 3
    ),
    "mean(exit_status,succeeded_inv)": lambda f: (
        (f["exit_status"] + f["succeeded_inv"]) / 2
    ),
    "max(exit_status,any_failure)": lambda f: max(f["exit_status"], f["any_failure"]),
}


def auc(scores, labels):
    pos = [s for s, y in zip(scores, labels) if y]
    neg = [s for s, y in zip(scores, labels) if not y]
    if not pos or not neg:
        return float("nan")
    order = sorted(range(len(scores)), key=lambda i: scores[i])
    ranks = [0.0] * len(scores)
    i = 0
    while i < len(order):
        j = i
        while j + 1 < len(order) and scores[order[j + 1]] == scores[order[i]]:
            j += 1
        for k in range(i, j + 1):
            ranks[order[k]] = (i + j) / 2 + 1
        i = j + 1
    rp = sum(r for r, y in zip(ranks, labels) if y)
    return (rp - len(pos) * (len(pos) + 1) / 2) / (len(pos) * len(neg))


def at(scores, labels, t):
    tp = sum(1 for s, y in zip(scores, labels) if s >= t and y)
    fp = sum(1 for s, y in zip(scores, labels) if s >= t and not y)
    p = sum(labels)
    n = len(labels) - p
    return (
        tp / (tp + fp) if tp + fp else float("nan"),
        tp / p if p else float("nan"),
        fp / n if n else float("nan"),
    )


def recall_at_precision(scores, labels, pmin):
    best = (0.0, None)
    for t in sorted(set(scores)):
        prec, rec, _ = at(scores, labels, t)
        if prec == prec and prec >= pmin and rec > best[0]:
            best = (rec, t)
    return best


def fit_logreg(X, y, l2=1e-2, iters=3000, lr=0.5):
    w = [0.0] * (len(X[0]) + 1)
    n = len(X)
    for _ in range(iters):
        g = [0.0] * len(w)
        for xi, yi in zip(X, y):
            z = w[0] + sum(wj * xj for wj, xj in zip(w[1:], xi))
            p = 1 / (1 + math.exp(-max(-30, min(30, z))))
            d = p - yi
            g[0] += d
            for j, xj in enumerate(xi):
                g[j + 1] += d * xj
        for j in range(len(w)):
            w[j] -= lr * (g[j] / n + (l2 * w[j] if j else 0))
    return w


def predict(w, x):
    z = w[0] + sum(wj * xj for wj, xj in zip(w[1:], x))
    return 1 / (1 + math.exp(-max(-30, min(30, z))))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("answers")
    a = ap.parse_args()
    recs = [json.loads(line) for line in open(a.answers)]
    recs = [r for r in recs if r.get("answers")]
    for view in ("wrap4k", "tail20"):
        rs = [r for r in recs if r["view"] == view]
        y = [r["label"] for r in rs]
        F = [features(r["answers"]) for r in rs]
        print(
            f"\n=== view {view}: n={len(rs)} failures={sum(y)} successes={len(y) - sum(y)}"
        )
        cands = {
            k: [f[k] for f in F]
            for k in NOUL + ["outcome_fail", "succeeded_inv", "clean_done_inv"]
            if k not in ("succeeded", "clean_done")
        }
        for name, fn in FUSIONS.items():
            cands[name] = [fn(f) for f in F]
        # Logistic combination on a session-grouped split.
        cols = NOUL + ["outcome_fail"]
        dev = [
            hashlib.sha1(r["session"].encode()).digest()[0] < 154 for r in rs
        ]  # ~60 %
        Xd = [[f[c] for c in cols] for f, d in zip(F, dev) if d]
        yd = [yy for yy, d in zip(y, dev) if d]
        w = fit_logreg(Xd, yd)
        test_idx = [i for i, d in enumerate(dev) if not d]
        print(
            f"logreg (dev n={len(yd)}, test n={len(test_idx)}): weights "
            + ", ".join(f"{c}={wj:+.2f}" for c, wj in zip(["bias"] + cols, w))
        )
        rows = []
        for name, sc in cands.items():
            rows.append((name, sc, y))
        lr_scores = [predict(w, [F[i][c] for c in cols]) for i in test_idx]
        rows.append(("logreg [test split only]", lr_scores, [y[i] for i in test_idx]))
        print(
            f"{'score':40} {'AUC':>5} {'R@P90':>6} {'R@P95':>6} | {'@0.5 P/R/FPR':>17} | {'@0.8 P/R/FPR':>17}"
        )
        for name, sc, yy in sorted(rows, key=lambda r: -auc(r[1], r[2])):
            r90, _ = recall_at_precision(sc, yy, 0.90)
            r95, _ = recall_at_precision(sc, yy, 0.95)
            p5, rc5, f5 = at(sc, yy, 0.5)
            p8, rc8, f8 = at(sc, yy, 0.8)
            print(
                f"{name:40} {auc(sc, yy):5.3f} {r90:6.2f} {r95:6.2f} | {p5:4.2f}/{rc5:4.2f}/{f5:5.3f} | {p8:4.2f}/{rc8:4.2f}/{f8:5.3f}"
            )
        hits = [panel_hits(r["tail"]) for r in rs]
        pr = sum(1 for h, yy in zip(hits, y) if h and yy) / max(1, sum(y))
        fp = sum(1 for h, yy in zip(hits, y) if h and not yy) / max(1, len(y) - sum(y))
        print(
            f"error-panel pre-filter: recall on failures {pr:.2f}, fires on successes {fp:.2f}"
        )
    print(
        f"\nlatency p50 {sorted(r['ms'] for r in recs)[len(recs) // 2]} ms",
        file=sys.stderr,
    )


def deployment_report(path, fit_view="wrap4k"):
    """Fit the logistic score on FIT_VIEW's dev split, then measure it once
    on both views' test splits at the per-surface thresholds, with the
    precision to expect at realistic failure base rates."""
    recs = [json.loads(line) for line in open(path)]
    recs = [r for r in recs if r.get("answers")]
    cols = NOUL + ["outcome_fail"]

    def split(view):
        rs = [r for r in recs if r["view"] == view]
        dev = [hashlib.sha1(r["session"].encode()).digest()[0] < 154 for r in rs]
        return rs, dev

    rs, dev = split(fit_view)
    X = [[features(r["answers"])[c] for c in cols] for r in rs]
    y = [r["label"] for r in rs]
    w = fit_logreg([x for x, d in zip(X, dev) if d], [t for t, d in zip(y, dev) if d])
    print("\n=== deployment: logistic weights (fit on %s dev split)" % fit_view)
    print("bias = %.4f" % w[0])
    for c, wj in zip(cols, w[1:]):
        print(
            "%s = %.4f" % ("outcome:failure+partial" if c == "outcome_fail" else c, wj)
        )
    for view, t in (("wrap4k", 0.5), ("tail20", 0.8)):
        rs, dev = split(view)
        test = [i for i, d in enumerate(dev) if not d]
        sc = [predict(w, [features(rs[i]["answers"])[c] for c in cols]) for i in test]
        yy = [rs[i]["label"] for i in test]
        p, rc, fpr = at(sc, yy, t)
        print(
            f"{view} test n={len(test)} @{t}: precision {p:.2f} recall {rc:.2f} FPR {fpr:.3f} AUC {auc(sc, yy):.3f}"
        )
        for base in (0.05, 0.10, 0.39):
            prec = (
                rc * base / (rc * base + fpr * (1 - base))
                if rc * base + fpr * (1 - base)
                else float("nan")
            )
            print(f"   at a {base:.0%} failure base rate: precision {prec:.2f}")


if __name__ == "__main__":
    main()
    deployment_report(sys.argv[1])
