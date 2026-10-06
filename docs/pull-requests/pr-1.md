---
type: pull_request
state: closed (merged)
branch: feat/v1-wrapper → main
created: 2026-10-05T13:42:10Z
updated: 2026-10-05T14:13:07Z
author: gerchowl
author_url: https://github.com/gerchowl
url: https://github.com/gerchowl/watcher-s1/pull/1
comments: 1
labels: none
assignees: none
milestone: none
projects: none
merged: 2026-10-05T14:13:04Z
synced: 2026-10-06T15:23:47.059Z
---

# [PR 1](https://github.com/gerchowl/watcher-s1/pull/1) feat: watcher-s1 v1, a truthful wrapper with tier 0-2 detection

v1 of watcher-s1 as designed in gerchowl/g-fleet#244 ("Updated recommendation" plus the two follow-up decisions): a Rust wrapper, `watcher-s1 [opts] -- cmd args…`.

## What's in it
- **Wrap and truthful exit.** The child runs in its own process group: under a PTY by default (its own session, PTY as controlling terminal), or with plain pipes via `--pipe`. Output is teed through unchanged (no CRLF rewrite) into a 16 KB ring. The exit code passes through; death by signal re-raises the same signal with its default action, so a shell sees 128+n. The verdict never touches the exit code.
- **Tier 0.** `--silence` timer. `--timeout` TERMs the whole pgid, then KILLs it after `--kill-grace`, and the group is KILLed again after the child is reaped so grandchildren go too. Process-state sampler: Linux `/proc/<pid>/stat` D state plus `wchan`; darwin `ps -o stat=` U state. Every sample runs on a worker under `--probe-timeout`. A timed-out probe counts as "maybe wedged", and no new probe starts while one is stuck. `/proc` reads are limited to `stat` and `wchan` (never `cmdline`, which can hang); `/nix/store` is never touched.
- **Tier 1.** Prompt regex on the unterminated last line (plus a short context window for ssh's host-key question), once output has been quiet for `--prompt-after`. A weak error panel decides whether an exit-0 tail is worth a System One call.
- **Tier 2.** System One at event time only: on exit (always for a failure; for exit 0 only when the panel trips), and at the silence threshold (async, so the tee never blocks). One request per state carries all questions. Ordered endpoints, a per-endpoint breaker persisted under `$XDG_STATE_HOME` (so it works across processes), DNS resolved once under the deadline, fail open.
- **Config precedence** exactly as decided: CLI > `SYSTEMONE_URL` > XDG > `/etc` > off (logged once). No endpoint is compiled in. `watcher-s1 config` shows each value's source.
- **Events:** `event.schema.json` (schema 1), sinks `--events FILE` / `--events-fd N` / `watcher-s1: `-prefixed stderr, and `caused_by` chained via `WATCHER_S1_PARENT`.
- **devkit integration:** Rust toolchain in the dev shell; `cargo fmt --check` and `cargo clippy -D warnings` as flake-generated hooks, so they run in `just precommit` and CI lint. `just test` runs `cargo test --locked`. `packages.default` is exposed for x86_64/aarch64-linux and aarch64/x86_64-darwin. A CI extension builds the package on Linux and macOS and runs the suite on macOS.

## Decisions taken along the way (deviations to review)
1. **Additive event fields:** `run_id` (needed so `caused_by` can point at something), `reason` (what triggered the event), and optional `proc` / `prompt`. The schema allows unknown fields; nothing from the specified shape was removed.
2. **When Kev is called at exit:** "on exit" and "on exit 0 whose tail looks failing" are read as: always on a failure (to add evidence and confidence), and on exit 0 only when the error panel trips. A clean exit 0 makes no call.
3. **Silence and Kev:** when Kev's fused score at the silence threshold is ≥ threshold, the event is `failing/silence` rather than `stalled/silence`.
4. **Breaker persisted on disk** rather than in memory. A watcher makes only a handful of calls, so an in-memory breaker would never trip. Corrupt or unwritable state degrades to "closed".
5. **http:// only.** System One is on the tailnet, and no TLS stack keeps the binary small and the deadline ours to enforce.
6. **Not a musl build.** `pkgsStatic` would make every consumer build musl gcc/rustc from source. The package is one binary whose only runtime dependencies are glibc/libgcc.
7. **Not in v1:** `--log FILE` passive mode (optional in the design; it has no exit to judge, so it would only add stall/prompt events). Also not here: an "on prompt: cancel" action. watcher-s1 detects and never answers; acting is the agent's or gateway's call.

## Finding from the real Kev run (needs a decision upstream, not here)
Fused ≥ 0.8 is reached by tails that **end in an error**: compile error 0.89, nix builder failure 0.96, Python traceback 0.94. A **test-runner summary** ("1 failed, 6 passed", `test result: FAILED`) fuses around 0.45–0.5, because `clean_done` reads "reached the summary" as a clean end (`clean_done` ≈ 0.7–0.8). g-fleet#244's 5/5 masked-pipe precision was measured on `failing ≥ 0.8` alone. The default stays as decided (fused, 0.8). The question file can switch to `positive = ["failing"]` without a rebuild. Documented in the README.

## Tests
39 unit + 36 integration tests (fake local System One), all run by `just test`. Covered: exit 3; SIGTERM/SIGKILL/SIGINT re-raise in PTY and pipe mode; 128+n seen by a shell; grandchildren killed by the pgid kill in both modes; KILL escalation when TERM is trapped; silence → resume; prompts on stdout and on `/dev/tty`; probe timeout → `stalled/blocked`; `caused_by` across nested watchers; all three sinks; config precedence; breaker across runs; hung, 500 and dead endpoints; first-healthy ordering; custom question files. The real Kev checks (`tests/kev.rs`, opt-in via `SYSTEMONE_URL`, sequential) pass against sage.



---
---

# Comments (1)

## [Comment #1](https://github.com/gerchowl/watcher-s1/pull/1#issuecomment-5996244056) by [@gerchowl](https://github.com/gerchowl)

_Posted on October 5, 2026 at 02:13 PM_

A fresh-context review found 3 blockers, all in paths the original CI never exercised. All three are fixed in 4ff7d2b, plus the nits:
- **Blocking tty stdin read** froze the loop: no output drain, no timeout, SIGTERM ignored. stdin is now read only on POLLIN, and forwarded keystrokes are buffered and written on POLLOUT.
- **Staircased output** in interactive mode: raw mode now keeps OPOST|ONLCR.
- **Inherited SIG_IGN** (nohup, `cmd &`) was overridden. It is now kept and not forwarded.
- **Nits:** `--pipe` reclaims the terminal when exec fails; breaker half-open admits exactly one caller across processes (under flock); stale samples are dropped; huge durations are rejected; errno is saved in the handler.

New `tests/tty.rs` runs the watcher on a real PTY as its controlling terminal; 6 of its 7 tests fail on the pre-fix code. On darwin the mode comparison ignores PENDIN, which XNU sets on every return to canonical mode.

Known and accepted: writes to our own stdout are blocking, so a stalled downstream reader also stalls the timers. That fd is shared, so making it non-blocking would leak O_NONBLOCK to other processes.

---
---

## Commits

### Commit 1: [e5101bf](https://github.com/gerchowl/watcher-s1/commit/e5101bf46349c7824cd0b357478a1b8214ef50ac) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 01:40 PM
build: add the Rust toolchain, cargo hooks and the watcher-s1 package to the flake, 1430 files modified (.gitignore.project, Cargo.lock, Cargo.toml, flake.lock, flake.nix, justfile.project, rustfmt.toml)

### Commit 2: [de46451](https://github.com/gerchowl/watcher-s1/commit/de464515e39634280f92b2a8adc736bbe5f89d13) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 01:40 PM
feat: watcher-s1 v1, a truthful wrapper with tier 0-2 detection, 3294 files modified

### Commit 3: [2e32adf](https://github.com/gerchowl/watcher-s1/commit/2e32adff820ce6a219d7879a4399051e5bcb60f3) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 01:40 PM
test: cover exit fidelity, process-group kill, timers, prompts and System One, 988 files modified (tests/common/mod.rs, tests/kev.rs, tests/s1.rs, tests/wrap.rs)

### Commit 4: [68704aa](https://github.com/gerchowl/watcher-s1/commit/68704aaed6883d9590fa325d95924e2d2bcb116b) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 01:40 PM
docs: document usage, events, System One config and Claude Code use, 280 files modified (.github/workflows/ci-extension.yml, README.md)

### Commit 5: [66b8021](https://github.com/gerchowl/watcher-s1/commit/66b80218fa6d29afaa69ccf04526b7e475e75081) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 01:41 PM
chore: re-render the devkit scaffold for the Rust project, 17 files modified (.gitignore, .gitignore.project, .vig-os)

### Commit 6: [4ff7d2b](https://github.com/gerchowl/watcher-s1/commit/4ff7d2bbae5ac6e90ac6ab706f5031b8b2fd8e96) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 01:52 PM
fix: interactive stdin, terminal output, ignored signals and breaker half-open, 455 files modified (src/breaker.rs, src/cli.rs, src/s1.rs, src/supervise.rs, tests/tty.rs, tests/wrap.rs)

### Commit 7: [e60d1a0](https://github.com/gerchowl/watcher-s1/commit/e60d1a0e57c048136589054db339ec48ddc8b8b5) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 01:59 PM
test: check terminal-mode restore from inside the session, 26 files modified (tests/tty.rs)

### Commit 8: [a185bbc](https://github.com/gerchowl/watcher-s1/commit/a185bbceb2abda3e5a89d5d1c7d68008b623b73d) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 02:06 PM
test: ignore the XNU PENDIN flag when comparing terminal modes, 10 files modified (tests/tty.rs)
