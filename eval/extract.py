#!/usr/bin/env python3
"""Extract labelled Bash calls from Claude Code transcripts.

Every Bash tool_use is paired with its tool_result by id. Labels come from
real signals only:
  fail  - the result is an error whose text starts with "Exit code N" (N>0);
          that harness-added prefix line is stripped from the output
  ok    - a successful result (exit 0), not interrupted
Background launches, timeouts, interrupts and validation errors are skipped.

`piped` marks commands whose status may be a filter's (`... | tail`). Their
exit code is not ground truth, so the primary eval excludes them. A piped
exit-0 row is promoted to `masked_fail` when the very next Bash call in the
same session re-runs the same command with the filter removed and THAT fails
(rule R1 of g-fleet#244).

Usage: extract.py OUT.jsonl ROOT [ROOT...]   (ROOT holds <project>/<session>.jsonl)
Output rows carry raw command output: keep them local, never commit them.
"""

import glob
import hashlib
import json
import os
import re
import sys

EXIT_RE = re.compile(r"^(?:Error: )?Exit code (\d+)\n?")
FILTERS = (
    "tail head grep egrep fgrep rg ag sed awk gawk cut sort uniq wc tee less more cat jq yq column tr fold nl paste"
).split()
PIPE_RE = re.compile(
    r"(?<!\|)\|(?![|&])\s*(?:\S+=\S+\s+)*(?:/\S*/)?(" + "|".join(FILTERS) + r")\b"
)


def text_of(content):
    if isinstance(content, list):
        return "\n".join(x.get("text", "") for x in content if isinstance(x, dict))
    return content or ""


def unpiped(cmd):
    """The command with every `| filter ...` stage removed."""
    return re.sub(r"\s*(?<!\|)\|(?![|&])[^|;&]*", "", cmd).replace("2>&1", "").strip()


def norm(cmd):
    return re.sub(r"\s+", " ", cmd.replace("2>&1", "")).strip()


def session_calls(path):
    uses, results = [], {}
    for line in open(path, errors="replace"):
        try:
            r = json.loads(line)
        except ValueError:
            continue
        m = r.get("message") or {}
        content = m.get("content")
        if not isinstance(content, list):
            continue
        for it in content:
            if not isinstance(it, dict):
                continue
            if it.get("type") == "tool_use" and it.get("name") == "Bash":
                uses.append(it)
            elif it.get("type") == "tool_result":
                results[it.get("tool_use_id")] = (it, r.get("toolUseResult"))
    for u in uses:
        res = results.get(u.get("id"))
        inp = u.get("input") or {}
        cmd = inp.get("command") or ""
        if not res or not cmd or inp.get("run_in_background"):
            continue
        it, tur = res
        text = text_of(it.get("content"))
        if it.get("is_error"):
            m = EXIT_RE.match(text)
            if not m or int(m.group(1)) == 0 or "Command timed out" in text:
                continue
            yield {
                "cmd": cmd,
                "outcome": "fail",
                "exit_code": int(m.group(1)),
                "output": text[m.end() :],
            }
        elif isinstance(tur, dict):
            if tur.get("interrupted"):
                continue
            out = tur.get("stdout") or ""
            if tur.get("stderr"):
                out = (out + "\n" + tur["stderr"]) if out else tur["stderr"]
            yield {"cmd": cmd, "outcome": "ok", "exit_code": 0, "output": out}


def main():
    out_path, roots = sys.argv[1], sys.argv[2:]
    seen_files, seen_rows, n = set(), set(), 0
    with open(out_path, "w") as out:
        for root in roots:
            for path in glob.glob(os.path.join(root, "*", "*.jsonl")):
                real = os.path.realpath(path)
                if real in seen_files:
                    continue
                seen_files.add(real)
                calls = list(session_calls(path))
                for c in calls:
                    c["piped"] = bool(PIPE_RE.search(c["cmd"]))
                for i, c in enumerate(calls):
                    if c["piped"] and c["outcome"] == "ok" and i + 1 < len(calls):
                        nxt = calls[i + 1]
                        if (
                            nxt["outcome"] == "fail"
                            and not nxt["piped"]
                            and norm(nxt["cmd"]) == norm(unpiped(c["cmd"]))
                        ):
                            c["outcome"] = "masked_fail"
                    key = hashlib.sha1(
                        (c["cmd"] + "\0" + c["output"][:2048]).encode()
                    ).hexdigest()
                    if key in seen_rows:
                        continue
                    seen_rows.add(key)
                    c["session"] = os.path.basename(path)[:-6]
                    out.write(json.dumps(c) + "\n")
                    n += 1
    print(f"{n} rows from {len(seen_files)} transcripts", file=sys.stderr)


if __name__ == "__main__":
    main()
