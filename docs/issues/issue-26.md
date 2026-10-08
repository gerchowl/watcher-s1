---
type: issue
state: closed
created: 2026-10-07T12:54:39Z
updated: 2026-10-07T16:05:07Z
author: gerchowl
author_url: https://github.com/gerchowl
url: https://github.com/gerchowl/watcher-s1/issues/26
comments: 1
labels: none
assignees: none
milestone: none
projects: none
parent: none
children: none
synced: 2026-10-08T08:43:13.311Z
---

# [Issue 26]: [0.2.0 release review: MCP stop/state safety, non-blocking diagnostics, resource bounds](https://github.com/gerchowl/watcher-s1/issues/26)

Independent release review (codex, GPT-6-Astra) of `v0.1.0..dev` at 7441043, to be released as 0.2.0. Verdict: **CHANGES**. The full review follows; findings are fixed in PRs into `dev` that reference this issue.

<details><summary>Full review</summary>

# Release review: watcher-s1 0.2.0

Reviewed `v0.1.0..74410437783f23a05c8c58e6e06346a953536556`, including the binding decisions in `codex-spec.md`. Read README and the agent guide first. Bot issue/PR mirrors and unrelated scaffold changes are excluded. No implementation files were changed.

The release needs changes. The principal defects are unsafe MCP signalling, metadata path traversal, incomplete process-group stopping, and blocking operations in paths documented as responsive. Passing integration tests do not cover these failures.

## Blocking

### B1 — `watch_stop` can signal unrelated processes and kill the watcher itself

**Severity: P1.** `src/mcp/runs.rs:283`, `src/mcp/runs.rs:310`, `src/mcp/runs.rs:327`, `src/mcp/runs.rs:334`; `src/mcp/mod.rs:274`.

Liveness is just `kill(pid, 0)`. Metadata stores no process birth identity, and `signal_watcher` signals the stored number without checking its identity. After a watcher dies without a final event and its PID is reused, status reports the unrelated process as running and stop sends it TERM. Restarting the MCP server makes this a normal stale-state scenario, not merely a malicious-file scenario.

**Confirmed:** created a run fixture with an empty events file and `watcher_pid` set to an unrelated, disposable `sleep 60`. `watch_status` returned `running`; `watch_stop` terminated that sleep with SIGTERM (`returncode=-15`). This models the indistinguishable metadata left after PID reuse; I did not force kernel PID reuse.

Escalation is independently unsafe: `job_pgid` trusts the last event's `pgid`, casts an arbitrary i64 to i32, and caches it across a grace period. Its guard compares against the **MCP server's** process group, not the detached watcher's group. A crafted event can therefore nominate another group's PGID, including the watcher's own session/group. `watcher_pid` also accepts 0 and values that cast to negative PID selectors. Those dangerous broad selectors were not executed in this review.

**Fix:** validate numeric ranges and bind every run to verified process identity, including across restarts. Prefer an authenticated control connection to the surviving supervisor, which already owns the child and can perform group escalation before reaping. Use OS process handles/birth identity where appropriate; a PID existence check or executable-name check is insufficient. Never authorize group killing from editable event JSON. Explicitly protect the watcher's group and test stale PIDs, stale PGIDs, forged group fields, special PID values, and the check-to-signal race.

### B2 — A forged metadata ID escapes `runs/` and overwrites external files

**Severity: P1.** `src/mcp/runs.rs:140`, `src/mcp/runs.rs:249`, `src/mcp/runs.rs:256`; `src/mcp/mod.rs:197`, `src/mcp/mod.rs:247`.

Only the tool's lookup ID is validated. Deserialized `meta.id`, `meta.events`, and `meta.log` are trusted. `watch_wait` subsequently uses `meta.id` to construct the cursor path without validation. An absolute metadata ID discards the root in `PathBuf::join`; `../` escapes it. Symlinked run directories/cursors introduce additional escapes.

**Confirmed:** kept the valid lookup directory `runs/stale`, changed its `meta.id` to `../outside`, and created `<state>/outside/cursor` containing `DO NOT OVERWRITE`. Calling `watch_wait {id:"stale", until:"final", timeout_s:0}` on a valid final-event fixture changed the external cursor to `1`.

**Fix:** use the validated directory-entry ID as authoritative; reject mismatched metadata IDs and derive events/log/cursor paths from that directory. Open state files relative to trusted directory handles with no-follow/type checks and atomic cursor replacement. Validate ownership/permissions for the intended state trust model. Add traversal, absolute-path, symlink, and metadata-mismatch tests. The confirmed write escape is separate from pruning: the simple prune traversal/symlink cases tested below did preserve external files.

### B3 — A stalled stderr event sink still freezes the hard timeout

**Severity: P1.** `src/outbox.rs:118`, `src/supervise.rs:74`, `src/supervise.rs:685`; claims at `README.md:62` and `CHANGELOG.md:29`.

The event writer is asynchronous, but supervisor diagnostics still write synchronously to stderr. Timeout logs **before** sending TERM. If the stderr event writer holds the stderr lock while blocked on a full pipe, the supervisor blocks acquiring that lock; even with another event sink, writing the diagnostic to a full stderr pipe blocks. The new overflow warning in `Outbox::park` has the same problem and directly contradicts its “Never blocks” contract.

**Confirmed:** ran `--no-s1 --pipe --heartbeat 1s --silence 0 --timeout 3s --kill-grace 0.1s -- sleep 60` with stderr connected to a pipe, waited for child startup, then filled that pipe and stopped reading. After more than 4.5 seconds, both child and watcher were still alive. Draining stderr immediately allowed the timeout and signal exit to complete.

The existing stalled-event test (`tests/heartbeat.rs:559`) uses `-q`, an events FD, and `/dev/null` stderr, avoiding both failure paths.

**Fix:** route diagnostics through a bounded, nonblocking transport as well; overflow diagnostics must never synchronously write from the supervisor. Preserve signal/timer execution even when all external output is blocked. Add the default, non-quiet stderr-sink regression test. Blocking at final flush is documented and is a separate, intentional behavior.

### B4 — One blocked probe or state-file read freezes the entire MCP server

**Severity: P1.** `src/mcp/runs.rs:315`, `src/mcp/runs.rs:144`, `src/mcp/runs.rs:250`; `src/mcp/mod.rs:306`, `src/mcp/mod.rs:540`.

The current-thread Tokio runtime calls synchronous `ps ... .output()` during stop. The two-second loop deadline is checked only after the subprocess returns, so it does not bound `ps`. Meanwhile pings, other tool calls, Channels tasks, cancellation, and timers cannot run. `fs::read_to_string` for metadata/cursors is likewise unbounded and does not reject FIFOs: a crafted `meta.json` FIFO can block even startup pruning.

**Confirmed probe case:** put a temporary `ps` fixture on PATH that marked its startup and slept for two seconds. Sent `watch_stop` for a live fixture with no events, then `ping` after the marker appeared. Neither request received a response during the following half-second. An indefinitely blocked probe produces an indefinite server freeze.

**Fix:** execute external probes with a real wall-clock deadline, kill/reap on timeout, and keep blocking operations off the runtime thread. Bound filesystem work and reject nonregular state files with nonblocking/no-follow opens. Do not assume wrapping an already-blocking future in a Tokio timeout makes it interruptible. Also remove or explicitly package the runtime `ps` dependency: `flake.nix:68` supplies procps as a build input, not a runtime PATH guarantee.

### B5 — `watch_stop` reports success while descendants continue running

**Severity: P1.** `src/mcp/mod.rs:285`, `src/mcp/mod.rs:320`; `src/supervise.rs:1153`.

`ended_within` treats either the leader's final event or the watcher's disappearance as proof that the job ended. A forwarded TERM is not the supervisor's timeout/prompt-cancel mode, so `w.we_killed()` is false and the supervisor does not perform its pre-reap group cleanup. If the leader dies on TERM but a descendant ignores TERM, stop sees the final event and skips SIGKILL altogether.

**Confirmed:** started a Python leader that spawned a same-group child, waited for that child to install `SIGTERM=SIG_IGN` and write its PID, then called `watch_stop` with one-second grace. Result: `ended:true`, `escalated_to_sigkill:false`, final signal 15. The grandchild was still alive and required separate cleanup.

**Fix:** make explicit MCP stop a supervisor-owned group shutdown operation, preserving the group identity until cleanup completes. A leader verdict is not proof that the whole group is gone. Avoid fixing this by blindly killing a cached PGID after the final event; that introduces B1's reuse race. Test a cooperative leader with an uncooperative descendant, not only a group whose leader also ignores TERM.

### B6 — Event reads and responses have no effective resource bound

**Severity: P1.** `src/follow.rs:150`, `src/follow.rs:568`, `src/follow.rs:580`; `src/mcp/mod.rs:199`, `src/mcp/mod.rs:231`, `src/mcp/runs.rs:261`.

`LineBuf` grows until newline with no byte limit, repeatedly scanning the growing prefix. `EventReader::poll` drains to EOF/final with no byte/event/time budget and accumulates all events. MCP wait retains the entire history; status and each stop poll reread it from the beginning. `MAX_BATCH=100` limits only the final selection for `until:next`, after the expensive allocations; `until:final` can return unlimited edge events.

**Failure scenario (source-confirmed, no intentional OOM):** a large crafted file containing one unterminated line consumes memory and increasingly expensive scans. A growing stream that stays ahead of the reader can prevent `poll` returning, so even `timeout_s:0` cannot bound the call and the single-thread server stops servicing other requests. Ordinary long heartbeat histories also make every status request unnecessarily expensive. Repeated distinct run IDs additionally grow the follower's parent map.

**Fix:** impose explicit line and history limits, discard oversized lines through their newline, budget each polling pass, and yield between bounded batches. Persist/use incremental offsets with file identity checks, maintain a compact status summary, and cap response bytes for both wait modes. Define behavior for excessively many run IDs. Add large/unterminated-line and sustained-producer responsiveness tests.

## Should-fix

### S1 — Concurrent waits duplicate events and can move the cursor backwards

**Severity: P2.** `src/mcp/mod.rs:197`, `src/mcp/mod.rs:247`; `src/mcp/runs.rs:256`; `README.md:346`.

Each wait snapshots the cursor before awaiting. Concurrent calls use the same snapshot, and an older timed-out `until:final` call can later write its old cursor over a newer call's progress. Multiple server processes sharing the state directory have the same race. Cursor writes are non-atomic and errors are silently ignored. Even sequential calls deliberately repeat the final verdict with `already_seen`, contrary to the README's unconditional “no event is delivered twice.” A disconnect after advancing the cursor but before delivery can conversely lose an event to that client.

**Confirmed:** issued two outstanding `until:next` calls for a live, empty fixture, then appended one event. Both responses returned `seq:1`.

**Fix:** serialize per-run cursor transactions, including across server instances, use atomic monotonic updates, and propagate persistence errors. Decide/document cursor ownership and delivery semantics; restart-persistent state alone cannot guarantee exactly-once transport delivery. Test overlapping waits, cancellation/disconnects, persistence failure, and two servers.

### S2 — Delayed heartbeat verdicts can reverse observable state transitions

**Severity: P2.** `src/supervise.rs:490`, `src/supervise.rs:514`, `src/supervise.rs:423`, `src/supervise.rs:1445`.

A heartbeat is timestamped and captures state at its tick, then remains outside the outbox while its S1 verdict is pending. Edge/resumed events bypass it. For example, a stalled heartbeat waits for S1, output resumes and emits `progressing/resumed`, then the older `stalled/heartbeat` is appended afterward. An agent consuming events in file order can conclude the job stalled again. The outbox preserves enqueue order, not event creation order.

**Fix:** release an outstanding heartbeat fail-open before enqueueing a later transition, or use a bounded ordered emission mechanism that cannot delay transitions. Test a delayed heartbeat verdict across silence, prompt, and resumed transitions. This is a source-level finding; network-backed reproduction was unavailable in this sandbox.

### S3 — A metadata write error leaves an untracked detached command running

**Severity: P2.** `src/mcp/runs.rs:210`, `src/mcp/runs.rs:228`.

The watcher starts and is handed to a reaper before `meta.json` is written. If that write fails (disk full, filesystem error, changed permissions), `watch_start` returns an error but the command continues detached. It is invisible to `watch_list`, later stop/wait calls cannot recover it, and pruning skips its missing/damaged metadata. An agent retrying the failed start can run the operation twice.

**Fix:** make run creation transactional. Persist a recoverable starting record before launch and atomically publish identity; preferably hold the command behind a startup handshake until metadata is durable. On failure, clean up using the owned process identity and report whether anything ran. Add a metadata-write fault-injection test. Source-confirmed; not fault-injected here.

### S4 — Rotation discovery can starve behind writes to an old file

**Severity: P2.** `src/follow.rs:493`.

Rotation is checked only when no retained file made progress. If the producer keeps the renamed file busy, the new pathname is never opened. A final event for the locked run written to the replacement file can therefore remain unseen until timeout, despite the documented support for late writes to the old file. The rotation test covers an old file that becomes idle.

**Fix:** check the pathname on a bounded schedule independently of whether old descriptors produced data, then service all descriptors fairly. Test sustained writes to the old inode with the locked run's final event on the new inode. Document the existing 16-old-file retention limit as well. Source-confirmed, not stress-reproduced here.

### S5 — Startup-only pruning does not bound detached log growth

**Severity: P2.** `src/mcp/runs.rs:119`, `src/mcp/runs.rs:193`; `src/mcp/mod.rs:533`.

A detached `watch_start {cmd:["yes"]}` has no default deadline and writes `output.log` without a quota. A long-lived server never prunes completed runs again; active runs are exempt regardless of age. Thus the feature has age-based cleanup at startup, but no bound on disk consumption. Disk exhaustion also triggers S3 and can silently prevent final-event writes through the existing sink's error-swallowing behavior (`src/event.rs:274`).

**Fix:** establish configurable per-run/aggregate retention or quotas and ongoing cleanup, with an explicit policy for active runs and reserved capacity/error reporting for metadata and final events. Document that startup age pruning alone is not a storage bound. Avoid blindly rotating the event file without also updating MCP's reader/cursor strategy.

### S6 — Several new tests still race startup, reaping, or scheduling

**Severity: P2.** `tests/mcp.rs:242`, `tests/mcp.rs:344`, `tests/mcp.rs:375`, `tests/mcp.rs:403`; `tests/heartbeat.rs:116`.

Concrete slow-CI failure scenarios:

- The final event can be observed before the watcher exits or its reaper runs, so immediately requiring `alive:false` is racy.
- The TERM-ignoring test stops immediately after start; observing a forked child with `ps` does not prove its shell installed the trap. It can die on TERM and fail the SIGKILL assertion.
- The killed-watcher test runs one `ps` snapshot and assumes the command has already spawned.
- The fixed three-second job can finish before the first wait is processed, making the subsequent “must time out” assertion false.
- The heartbeat test requires each observed tick to be within 900 ms of its grid point. A valid implementation descheduled longer can violate that without violating coalescing behavior.

**Fix:** use readiness/gate files and observable state for startup, trap installation, and final/reaped states. Release fixed-duration jobs only after intermediate assertions. Keep deterministic grid/coalescing assertions in unit tests and use scheduler-tolerant integration bounds. These passed on this Linux run; macOS was not available. Add regressions for B1–B6, rather than treating current passing tests as proof of their advertised guarantees.

### S7 — The reviewed artifact still identifies itself as 0.1.0

**Severity: P2, required before publishing 0.2.0.** `Cargo.toml:3`, `Cargo.lock:1634`, `README.md:84`, `README.md:91`, `CHANGELOG.md:8`.

**Confirmed:** the built binary prints `watcher-s1 0.1.0`; MCP advertises the same Cargo package version. README install examples explicitly select v0.1.0, which lacks the new commands, while the guide requires at least 0.2.0. The checked-in prepare workflow freezes the changelog but has no Cargo version update; the binary workflow prints the version without comparing it to the tag.

**Fix:** update Cargo manifest/lock package versions and release install examples, and freeze/date the changelog as part of release preparation. Add an artifact-version-versus-tag check. This may appropriately happen in the release branch, but the exact reviewed HEAD is not yet a correctly versioned 0.2.0 artifact.

## Nits

### N1 — State-directory and pruning documentation omit relevant exceptions

**Severity: P3.** `README.md:341`; implementation at `src/main.rs:65`, `src/mcp/runs.rs:124`.

The README omits `WATCHER_S1_STATE_DIR`, which takes precedence over XDG/HOME, and says runs older than seven days are pruned without saying living watchers are retained. An agent can inspect the wrong directory or expect storage cleanup that does not occur.

**Fix:** state the complete precedence and the dead-watcher condition. Keep `mcp --help`, README, and guide aligned.

### N2 — The short guide overstates heartbeat S1 call frequency

**Severity: P3.** `docs/agent-guide.md:62`.

“Adds a System One call per tick, so costs one each” conflicts with the in-flight guard and breaker. With a slow endpoint or open breaker, ticks carry null without a fresh call, as the README correctly explains.

**Fix:** say “opts into verdict requests, at most one heartbeat request in flight; some ticks carry `s1:null`.” The drift tests validate flag names and event enums, not these behavioral statements.

## Spec checklist

Status refers to this candidate. “Met” is limited to the stated decision, not a blanket assertion that surrounding code is defect-free.

### Issue #18 — heartbeat

| Binding decision | Status | Evidence / qualification |
|---|---|---|
| 1. Same sink; recommend events file | Met | Shared outbox and README recommendation. Sink responsiveness has B3. |
| 2. Current episode state; always info | Met at capture time; deviates in delivery order | `episode_state` and heartbeat constructor implement the specified states/severity; S2 can deliver obsolete state after a transition. |
| 3. Heartbeat dedup segment | Met | Dedicated literal segment and unit/schema tests. |
| 4. Optional elapsed/byte/line/last-line/evidence fields | Met for normal output | Schema declares them; incremental bounded line tracker handles ANSI and split UTF-8; integration coverage passed. |
| 5. Monotonic grid, coalescing, minimum 1s, no silence reset | Met in scheduling logic | `next_tick`, CLI checks, and coalescing tests. B3 can still prevent the loop from running. |
| 6. S1 requires heartbeat; same breaker/deadline/fail-open; no state change | Met by inspection and worker unit tests | Shared client path and in-flight guard. Four network-backed integration tests could not run successfully because local binds are prohibited. S2 remains an ordering defect. |
| 7. Passive log heartbeats | Met | Log-mode integration test passed. |
| 8. Skip on-demand status | Met | No new signal/control-file operation. |
| 9. Schema remains 1; enum and compatibility warning | Met | Schema and README updated. |

Acceptance: timed heartbeat/final sequence, stalled-state heartbeats, minimum rejection, and missed-tick coalescing tests passed. No-S1-by-default is evident in code, but its fake-endpoint integration test failed at server setup, not at an assertion about the implementation. The stalled-sink acceptance needs B3's non-quiet stderr case.

### Issue #19 — guide, follow, drift test

| Binding decision | Status | Evidence / qualification |
|---|---|---|
| 1. Embedded guide, Cargo include, Nix extraSrcFiles, short and linked | Met | `src/lib.rs`, Cargo include list, flake additions; package listing includes guide/schema/README. Guide is 119 lines, reasonably close to the requested size. |
| 2. First-run lock, nested indentation, only its final ends follow; --new, missing file, partial/malformed lines, compact output, exit codes | Met for covered cases; rotation extension has S4 | Unit/integration tests passed, including partial UTF-8, malformed identity/exit, truncation, replacement, FIFO rejection, and timeout. Additional exit codes 1/3 are documented. |
| 3. Clap-backed flag scan and bidirectional schema/table drift | Met | Nine drift tests passed with both feature configurations; scanner covers all named subcommands including MCP and explicit foreign-flag allowlist. |
| 4. Land after heartbeat; release 0.2.0; external skill handoff after release | Implementation order met; release/handoff pending | Git log puts heartbeat before guide and MCP. S7 prevents calling this exact artifact 0.2.0. The external claude-config change/comment is post-release work and was not performed. |

### Issue #20 — MCP

The binding comment uses three bullets rather than numbered decisions; numbered here in their original order.

| Binding decision | Status | Evidence / qualification |
|---|---|---|
| 1. Official rmcp, default-on optional feature; no-default build; lockfile | Met | rmcp 3.5.1 pinned in Cargo.lock; default build works; no-default library, drift, follow and absent-MCP tests passed. No manifest change disables default features in release builds. |
| 2. Ship in 0.2.0 with #18/#19 | Implementation included; release pending | All three surfaces are present, but S7 and blocking defects remain. |
| 3. Implement after heartbeat/guide; reuse parsing and embedded guide | Met | Git history order, `EventReader`, `format_event`, and `GUIDE` reuse. |

Additional acceptance/surface checks:

- **Met by protocol tests:** initialize, tool listing/object schemas, start → wait with truthful nonzero exit, guide resource, opt-in Channel edge notifications and no heartbeat pushes, and no push without the flag.
- **Met by integration test:** detached run survives server SIGKILL; replacement server waits with a persisted sequential cursor. Concurrency/delivery guarantees need S1; stop safety needs B1/B5.
- **Deviates from intended reliable supervision:** blocking probes/state reads and unbounded parsing (B4/B6), metadata transaction failure (S3), and incomplete storage bounds (S5).
- **Pruning:** old dead runs are pruned; alive ones retained. Confirmed that a mismatched metadata ID and a run-directory symlink did not cause external deletion in simple fixtures. This is not a proof against concurrent ancestor replacement; directory-relative containment should accompany B2's hardening.
- **Manual Claude Code Channels acceptance: unverified.** Registration instructions are present, but I found no recorded manual test result in the reviewed implementation/docs/tests. Run and record the requested interactive check before claiming that acceptance item complete.

## What you verified

- Read `README.md`, `docs/agent-guide.md`, `codex-spec.md`, the scoped git log/diff, new MCP/follow/outbox implementations, supervisor changes, schema, new tests, Cargo metadata, flake source filtering, and relevant release workflow steps. Examined installed rmcp 3.5.1 source for handshake and concurrent request dispatch.
- Checked the MCP lifecycle/tool surface against the official [MCP lifecycle specification](https://modelcontextprotocol.io/specification/2025-06-18/basic/lifecycle) and [tools specification](https://modelcontextprotocol.io/specification/2025-06-18/server/tools). The SDK handles negotiation; text-content tool results are permitted, and tool execution failures use `isError`. No separate wire-format defect was established.
- Compared `experimental: {"claude/channel": {}}` and `notifications/claude/channel` with string `content` and string-valued `meta` to the official [Claude Code Channels reference](https://code.claude.com/docs/en/channels-reference). The implemented notification shape matches that contract. This does not substitute for the requested interactive client test.
- `cargo test --locked --offline` initially failed because the configured `sccache` could not operate in the sandbox. With `RUSTC_WRAPPER=`: **92 unit tests, 9 drift tests, 21 follow tests passed; heartbeat: 9 passed, 4 failed at local TCP bind with EPERM**. Cargo stopped there, so this was not a passing full-suite run.
- Separately ran `RUSTC_WRAPPER= cargo test --locked --offline --test mcp`: **11/11 passed**. Ran `--test wrap --test tty`: **34/34 wrap and 7/7 TTY passed**.
- Ran `RUSTC_WRAPPER= cargo test --locked --offline --no-default-features --lib --test no_mcp --test drift --test follow`: **85 unit, 1 no-MCP, 9 drift, 21 follow passed**. The feature-disabled MCP command exits 2 with an explanation.
- Rebuilt with default features using `RUSTC_WRAPPER= cargo build --locked --offline`. `cargo package --locked --offline --list` includes all embedded inputs, including `docs/agent-guide.md`, `event.schema.json`, and `questions/builtin.toml`. Flake `extraSrcFiles` contains the guide, schema and README. No new shell interpolation was found in `watch_start`: command arguments are passed as argv behind `--`.
- Confirmed with disposable local fixtures: unrelated-PID TERM, metadata cursor traversal, non-quiet stalled-stderr timeout failure, surviving TERM-ignoring descendant, duplicated concurrent wait event, delayed-ps blocking of MCP ping, and basic prune containment. Test processes were cleaned up. Dangerous broadcast PID selectors and unrelated real processes were never targeted.
- `git diff --check v0.1.0..HEAD` over the reviewed non-mirror/non-scaffold paths passed. Only this review file was added to the working tree.
- **Not verified:** successful full network-backed suite, real System One, interactive Claude Code Channels, macOS behavior, Nix derivation build, cross-target release binaries, or a full packaged-crate installation. Package file inclusion and native offline builds were checked; they do not prove those other environments.

VERDICT: CHANGES

</details>

---

# [Comment #1]() by [gerchowl]()

_Posted on October 7, 2026 at 04:05 PM_

All findings addressed in #27 and #29 (S5's per-run output cap documented instead); shipped in v0.2.0.

