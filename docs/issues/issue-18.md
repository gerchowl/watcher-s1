---
type: issue
state: closed
created: 2026-10-07T10:24:29Z
updated: 2026-10-07T11:57:02Z
author: gerchowl
author_url: https://github.com/gerchowl
url: https://github.com/gerchowl/watcher-s1/issues/18
comments: 1
labels: none
assignees: none
milestone: none
projects: none
parent: none
children: none
synced: 2026-10-07T15:05:50.978Z
---

# [Issue 18]: [feat: --heartbeat DUR — periodic status event while the job runs](https://github.com/gerchowl/watcher-s1/issues/18)

## Motivation

Events are edge-triggered: `stalled`, `waiting_on_input`, `failing`, `progressing/resumed`, and one final event. A job that runs fine for 90 minutes emits nothing until it ends. For an agent (or a human) watching a long build, "nothing" looks the same as "the watcher died" or "the event file path is wrong". An agent then falls back to polling the log itself, which is exactly the busy-wait the watcher should replace.

Ask (from the fleet side): a status update at an interval the caller picks, next to the existing `--timeout` hard stop.

## Proposal

`--heartbeat DUR` (default off). Every DUR of wall time, emit one event:

```json
{"state": "progressing", "reason": "heartbeat", "severity": "info", "exit": null,
 "elapsed_ms": 5400000, "bytes_since_last": 18234, "lines_since_last": 312,
 "last_line": "[412/980] building foo", "evidence_tail": "…", "s1": null}
```

- `state` is the CURRENT state, not always `progressing`: during an open `stalled`/`waiting_on_input` episode the heartbeat repeats that state with `reason: heartbeat`, so a reader that only sees heartbeats still knows the situation.
- `dedup_key` gets a distinct state segment (or the `heartbeat` reason) so a gateway (g-fleet#188) can fold them.
- **No System One call by default.** Tier 2 is "event time only, never on a poll" (README), and a heartbeat is a poll. Optional `--heartbeat-s1` for callers who want a "still making progress?" verdict and accept one Kev call per interval; it goes through the same breaker and fails open.
- Schema stays 1 (new optional fields `elapsed_ms`, `bytes_since_last`, `lines_since_last`, `last_line`; a new `reason` value). Document `heartbeat` in the reason table.

## Open question: an on-demand status

"Tell me now" without waiting for the next tick. `SIGUSR1` is already forwarded to the child group, so it can't be reused. Options: a `--control FILE` path that the watcher polls (touch → one status event), or skip it in v1 and let callers pick a short `--heartbeat`. Recommendation: skip for now.

## Pitfalls

- Heartbeats in `--log` mode: emit them too (elapsed = since attach), same fields.
- With `--events` unset, heartbeats go to stderr and would interleave with the child's output under `--pipe`. Consider emitting heartbeats only to `--events`/`--events-fd`, or document it.
- Interval drift: tick from a monotonic clock, not "DUR after the last event".
- Must not reset the `--silence` timer (a heartbeat is our output, not the child's).

## Acceptance

- [ ] `--heartbeat 2s -- sleep 7` emits 3 heartbeat events then the final event; exit code unchanged.
- [ ] During a silence episode the heartbeat carries `state: stalled`.
- [ ] Heartbeats never trigger a System One request unless `--heartbeat-s1`.
- [ ] `event.schema.json` + README event table updated.

Context: g-fleet `watch` skill for agents (claude-config), g-fleet#244 (closed, v0.1.0).

---

# [Comment #1]() by [gerchowl]()

_Posted on October 7, 2026 at 10:31 AM_

## Review: approve, with these decisions settled

The proposal is sound. These resolve the open points so the implementation follows a fixed spec:

1. **Sink:** heartbeats go to the **same sink as every other event**: `--events`/`--events-fd`, or stderr when neither is set. One rule, no special case. The README recommends pairing `--heartbeat` with `--events`.
2. **`state`:** the current episode state (`stalled`/`waiting_on_input` while one is open, otherwise `progressing`). **`severity` is always `info`.** The edge event already raised the alert, and a heartbeat must not re-alert every tick.
3. **`dedup_key`:** uses `heartbeat` in the state segment, so all of a job's heartbeats fold into one key regardless of state.
4. **New optional fields** (top-level `additionalProperties` is already true; still declare them):
   - `elapsed_ms`: since start, or since attach in `--log` mode.
   - `bytes_since_last` and `lines_since_last`: child output since the previous heartbeat.
   - `last_line`: the last non-empty line, with ANSI escapes stripped, truncated to 200 chars, `null` if none.
   - `evidence_tail`: as usual.
5. **Ticks:** scheduled at `start + k·DUR` on the monotonic clock. If the loop was blocked past several ticks, emit **one** heartbeat and move on to the next future tick (no burst). **Minimum DUR is 1s** (clap error below). Heartbeats are never counted as child output: no silence-timer reset, no `resumed`.
6. **`--heartbeat-s1`:** `requires = "heartbeat"`. Same breaker, deadline and fail-open path as the silence-time call. The verdict is attached as `s1`; it **never changes `state`**.
7. **`--log` mode:** emits heartbeats too.
8. **On-demand status:** skipped (agreed). A short `--heartbeat` covers it.
9. **Schema stays 1:** `heartbeat` is added to the `reason` enum. A consumer validating strictly against the old enum would reject heartbeat events, but they only appear when the caller opts in. Note this in the schema description.

**Tests:** the acceptance list, plus no burst after a blocked loop, and `--heartbeat 0.5s` rejected.


