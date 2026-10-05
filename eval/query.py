#!/usr/bin/env python3
"""Ask System One every candidate question for every eval state.

Usage: query.py ROWS.jsonl OUT.jsonl URL [--fails N] [--oks N] [--seed S]

Samples clean (unpiped, non-empty) rows, builds two views per row (the last
4 KB as the wrapper sends it, and the last 20 lines as a `| tail -20` leaves
it for the PostToolUse judge), and sends ONE request per state with all
candidate questions. Strictly sequential: the endpoint host is wedge-prone.
Resumable: states already in OUT are skipped.
"""

import argparse
import hashlib
import json
import random
import re
import time
import tomllib
import urllib.request
from pathlib import Path

ANSI = re.compile(
    r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[()][A-Za-z0-9]"
)


def clean(s):
    s = ANSI.sub("", s)
    lines = []
    for line in s.split("\n"):
        line = line.rstrip("\r")
        if "\r" in line:
            line = line.rsplit("\r", 1)[1]
        lines.append(line.rstrip())
    return "\n".join(lines)


def tail_bytes(s, n=4096):
    b = s.encode()
    if len(b) <= n:
        return s
    t = b[-n:].decode("utf-8", "ignore")
    nl = t.find("\n")
    return t[nl + 1 :] if 0 <= nl < 200 else t


def tail_lines(s, n=20):
    return "\n".join(s.rstrip("\n").split("\n")[-n:]) + "\n"


def state(cmd, tail):
    return f"Command: {cmd}\n(The exit status is not shown.)\nLast output:\n{tail}"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("rows")
    ap.add_argument("out")
    ap.add_argument("url")
    ap.add_argument("--fails", type=int, default=574)
    ap.add_argument("--oks", type=int, default=900)
    ap.add_argument("--seed", type=int, default=244)
    a = ap.parse_args()

    qs = tomllib.loads(
        (Path(__file__).parent / "questions_candidates.toml").read_text()
    )["questions"]
    rows = [json.loads(line) for line in open(a.rows)]
    rows = [
        r
        for r in rows
        if not r["piped"]
        and len(r["output"].strip()) >= 20
        and r["outcome"] in ("ok", "fail")
    ]
    rng = random.Random(a.seed)
    fails = [r for r in rows if r["outcome"] == "fail"]
    oks = [r for r in rows if r["outcome"] == "ok"]
    rng.shuffle(fails)
    rng.shuffle(oks)
    sample = fails[: a.fails] + oks[: a.oks]

    done = set()
    if Path(a.out).exists():
        done = {(j["rid"], j["view"]) for j in map(json.loads, open(a.out))}
    with open(a.out, "a") as out:
        for i, r in enumerate(sample):
            rid = (
                r["session"]
                + ":"
                + hashlib.sha1((r["cmd"] + r["output"][:512]).encode()).hexdigest()[:12]
            )
            text = clean(r["output"])
            for view, tail in (
                ("wrap4k", tail_bytes(text)),
                ("tail20", tail_lines(text)),
            ):
                if (rid, view) in done:
                    continue
                body = json.dumps(
                    {"state": state(r["cmd"], tail), "questions": qs}
                ).encode()
                req = urllib.request.Request(
                    a.url, body, {"content-type": "application/json"}
                )
                for attempt in range(3):
                    try:
                        t0 = time.time()
                        resp = json.load(urllib.request.urlopen(req, timeout=30))
                        break
                    except Exception as e:  # noqa: BLE001 - retry, then record
                        resp = {"error": str(e)}
                        time.sleep(2 * (attempt + 1))
                rec = {
                    "rid": rid,
                    "view": view,
                    "label": 1 if r["outcome"] == "fail" else 0,
                    "exit_code": r["exit_code"],
                    "session": r["session"],
                    "cmd": r["cmd"][:300],
                    "tail": tail,
                    "answers": resp.get("answers"),
                    "error": resp.get("error"),
                    "ms": round((time.time() - t0) * 1000),
                }
                out.write(json.dumps(rec) + "\n")
                out.flush()
                time.sleep(0.05)
            if i % 100 == 0:
                print(f"{i}/{len(sample)}", flush=True)


if __name__ == "__main__":
    main()
