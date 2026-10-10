---
type: issue
state: open
created: 2026-10-09T13:40:19Z
updated: 2026-10-09T13:40:19Z
author: gerchowl
author_url: https://github.com/gerchowl
url: https://github.com/gerchowl/watcher-s1/issues/38
comments: 0
labels: agent-feedback
assignees: none
milestone: none
projects: none
parent: none
children: none
synced: 2026-10-10T08:19:57.747Z
---

# [Issue 38]: [Attach mode: watch running pids, files and agent panes with System One judgement (frozen agent TUI went unnoticed for 7 h)](https://github.com/gerchowl/watcher-s1/issues/38)

## Context
The MPECT architect supervises ~6 long-running coding agents (Codex and Claude) in flock panes, plus their long local jobs and GPU-lease jobs. Today a Codex TUI froze for ~7 h. It ignored all input, with its own waiter notices sitting unsent in the composer, and nothing flagged it, because the pane looked "idle and waiting". A second case: a `ci:` waiter waited >1 h on a PR whose merge conflict meant CI never started. A third: an agent lost its registered name after a harness restart, so its completion waiter could not reach it.

## What the ad-hoc supervisor now does (deterministic tiers)
- **Pane:** idle Codex pane with non-placeholder composer text for ≥15 checks → alert.
- **Transcript:** pane shows "busy", but its transcript file (Codex rollout jsonl matched by cwd, Claude session jsonl) has not grown for ~20 min → alert ("hung turn").
- **Waited process:** an agent is parked on `WAITING: process:<pid>`, the pid is alive, but its process group has burned no CPU for ~30 min → alert. It can't tell "stalled" from "legitimately waiting on a lease/network".

## Ask: an attach/observe mode for watcher-s1
`watcher-s1` already does output-silence stall detection plus a System One judgement, but only for commands it launches. Please add an **attach mode** for things already running:
1. `--pid <pid>`: watch a running process group: CPU time, open-file write growth, child churn, optional log file(s) to tail.
2. `--file <path>` (one or many): growth or silence on transcripts or logs.
3. `--pane <flock pane>` (or a generic "screen text" command): snapshot diff plus prompt and composer detection.

On tier-0/1 anomalies, run the **System One judge** with the snapshot (pane tail, last transcript lines, process tree and CPU, the declared WAITING target) to classify: healthy-waiting / stalled-job / frozen-UI / missed-resume / needs-human, with a suggested action. Emit the same JSON events, so supervisors can `follow` them instead of hand-rolling heuristics.

This would also let agents park on `WAITING: watch:<events-file>` and be resumed by the final event, with stall detection for free, instead of plain `kill -0` waiters.

