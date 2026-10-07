# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

### Added

### Changed

### Deprecated

### Removed

### Fixed

### Security

## [0.2.0] - TBD

### Added

- `--heartbeat DUR` (minimum `1s`) emits periodic `reason: heartbeat` status events (current episode state, severity `info`, `elapsed_ms`, `bytes_since_last`, `lines_since_last`, `last_line`) on a monotonic schedule, in `--log` mode too; `--heartbeat-s1` attaches a System One verdict to each; schema stays 1 with `heartbeat` added to the `reason` enum ([#18](https://github.com/gerchowl/watcher-s1/issues/18))
- `watcher-s1 guide` prints an agent-sized usage guide (`docs/agent-guide.md`), and
  `watcher-s1 follow EVENTS_FILE` streams an events file as compact lines until
  the run it locked onto finishes (`--timeout DUR` gives up with exit 3; it
  survives truncation and rotation of the file); a test keeps the guide and README in step
  with the CLI flags and the event schema ([#19](https://github.com/gerchowl/watcher-s1/issues/19))

- `watcher-s1 mcp [--channel] [--state-dir DIR]` serves the supervisor as an MCP server on stdio (official `rmcp` SDK, behind the `mcp` cargo feature, on by default): tools `watch_start`, `watch_wait`, `watch_status`, `watch_stop`, `watch_list` and the resource `watcher-s1://guide`; runs are detached watchers whose state lives under the state directory, so a later server can wait on them; `--channel` pushes edge events as Claude Code channel notifications ([#20](https://github.com/gerchowl/watcher-s1/issues/20))

### Changed

- `watch_stop` no longer signals processes: the supervisor owns a private control socket (hidden `--control PATH`: line-delimited JSON, `status` and `stop`) and performs the stop itself, with the same TERM, grace, KILL escalation as `--timeout`, including a group SIGKILL before it reaps the leader. The new final reason `stopped` (schema 1 enum) marks it; `watch_stop` loses its `signal` option (TERM only). Liveness in `watch_status`/`watch_wait` comes from that socket, never from a pid ([#26](https://github.com/gerchowl/watcher-s1/issues/26))
- `watch_wait` returns at most 100 events and 256 KiB per call (`more: true` when there is more), reads incrementally from a persisted byte offset, and is documented as delivering each intermediate event once per run (the final verdict repeats with `already_seen`); runs are pruned hourly while the server lives, and `output.log` is documented as unbounded unless a `timeout` is set ([#26](https://github.com/gerchowl/watcher-s1/issues/26))

### Fixed

- MCP: `watch_stop` could TERM an unrelated process (a reused or forged `watcher_pid`) or SIGKILL a group named by an event; it now talks only to the run's control socket ([#26](https://github.com/gerchowl/watcher-s1/issues/26))
- MCP: a forged `meta.json` (`../` or absolute id) could redirect the cursor write outside `runs/`; the directory-entry id is now authoritative, paths derive from the run directory, state files are opened `O_NOFOLLOW`, must be regular files and are size-bounded, and cursor writes are atomic ([#26](https://github.com/gerchowl/watcher-s1/issues/26))
- MCP: a stopped job's descendants that ignore TERM no longer survive `watch_stop` when the leader dies first ([#26](https://github.com/gerchowl/watcher-s1/issues/26))
- MCP: blocking filesystem and socket work (a FIFO `meta.json`, a hung probe) no longer freezes the server: it runs on the blocking pool with wall-clock bounds, and `ps` is gone ([#26](https://github.com/gerchowl/watcher-s1/issues/26))
- Event reading is bounded: lines over 1 MiB are discarded (one warning), a poll reads at most 4 MiB before yielding, the follower's parent map holds 4096 run ids, and the MCP server no longer rereads or accumulates a run's history ([#26](https://github.com/gerchowl/watcher-s1/issues/26))
- MCP: overlapping `watch_wait` calls, in one server or several, no longer get the same event or move the cursor backwards (per-run `flock` transactions; persistence errors are reported) ([#26](https://github.com/gerchowl/watcher-s1/issues/26))
- MCP: `watch_start` records the run (`starting`) before launching the watcher and marks it `failed` if the launch fails, so no detached command is left untracked ([#26](https://github.com/gerchowl/watcher-s1/issues/26))
- `follow` checks the pathname for rotation every 500 ms even while an old file keeps being written, so the final event on the new file is no longer starved; it keeps at most 16 old files ([#26](https://github.com/gerchowl/watcher-s1/issues/26))
- Documented the state directory precedence (`WATCHER_S1_STATE_DIR` first) and that pruning keeps runs whose watcher is alive ([#26](https://github.com/gerchowl/watcher-s1/issues/26))
- Event delivery no longer runs on the supervisor loop: a writer thread with a bounded queue keeps `--timeout`, signal forwarding and heartbeats responsive when the event sink (`--events-fd` pipe, stderr) is not read; heartbeats are shed under overload and report `heartbeats_dropped`, other events keep order via a bounded overflow queue. The final event may still block at exit on a sink nobody reads.
- `--heartbeat-s1` keeps at most one System One request in flight: after an overdue verdict, later heartbeats go out with `s1: null` and no new request starts until the slow one returns.
- No supervisor diagnostic or event write can block the loop any more: every `watcher-s1 (log):` line (and the overflow warning) goes through a bounded, nonblocking channel to one stderr writer thread shared with the stderr event sink (a full queue drops and counts), and a timeout sends TERM before it logs, so a stderr nobody reads no longer freezes `--timeout` ([#26](https://github.com/gerchowl/watcher-s1/issues/26))
- A heartbeat still waiting for its System One verdict is released fail-open before any later event is enqueued, so events never appear out of creation order (a stale `stalled` heartbeat after `resumed`) ([#26](https://github.com/gerchowl/watcher-s1/issues/26))
- The crate version is 0.2.0 (the binary and the MCP server reported 0.1.0), and the binary release workflow fails if `--version` differs from the tag ([#26](https://github.com/gerchowl/watcher-s1/issues/26))
- Heartbeat `last_line` is the true last non-empty output line (200 chars, ANSI stripped), tracked incrementally instead of read back from the 16 KiB evidence ring.

## [v0.1.0](https://github.com/gerchowl/watcher-s1/releases/tag/v0.1.0) - 2026-10-06

### Added

- `watcher-s1 [opts] -- cmd`: a truthful wrapper (own process group, PTY by default or `--pipe`, unchanged tee, exit code or re-raised signal) with tier 0 (silence timer, `--timeout` group kill, D/U-state sampler under probe timeouts), tier 1 (prompt regex, error panel) and tier 2 (System One at event time, ordered endpoints, persisted per-endpoint breaker, fail open) ([#1](https://github.com/gerchowl/watcher-s1/pull/1))
- Sideband events versioned by `event.schema.json` (schema 1) with `caused_by` chaining across nested watchers ([#1](https://github.com/gerchowl/watcher-s1/pull/1))
- System One configuration precedence: CLI > `SYSTEMONE_URL` > XDG > `/etc` > off; no endpoint compiled in ([#1](https://github.com/gerchowl/watcher-s1/pull/1))
- `watcher-s1 judge --posttooluse`, a Claude Code PostToolUse hook for masked pipes ([#2](https://github.com/gerchowl/watcher-s1/pull/2))
- `--on-prompt cancel`, `--log FILE` passive mode, `https://` endpoints ([#3](https://github.com/gerchowl/watcher-s1/pull/3))
- System One question set measured on 1,474 real labelled commands: ten questions combined into one logistic score (AUC 0.95 vs 0.88), thresholds per surface (wrapper 0.5, PostToolUse judge 0.8); eval harness in `eval/`, report in `docs/eval/questions-spike.md` ([#6](https://github.com/gerchowl/watcher-s1/pull/6))
- Release binaries (static musl Linux x86_64/aarch64, Apple-silicon macOS) attached to GitHub Releases through the devkit release train ([#5](https://github.com/gerchowl/watcher-s1/pull/5))

### Changed

- The flake builds through devkit's Rust pack (`mkRustProject`): toolchain pinned by `rust-toolchain.toml`, crane checks (clippy, fmt, nextest, doctest, doc), `cargo auditable` package ([#4](https://github.com/gerchowl/watcher-s1/pull/4))
- The wrapper judges every exit 0 that produced output instead of gating on an error regex, which caught 34 % of failures ([#6](https://github.com/gerchowl/watcher-s1/pull/6))
- Our stdout goes through a writer thread with a bounded queue: a stalled reader no longer stalls the timers ([#3](https://github.com/gerchowl/watcher-s1/pull/3))

### Fixed

- The final group SIGKILL after a timeout or prompt cancel is sent before reaping, so it cannot hit a reused pid ([#3](https://github.com/gerchowl/watcher-s1/pull/3))
- Interactive runs: stdin is read only when ready, output keeps CR/LF on the terminal, an inherited `SIG_IGN` (nohup) is respected ([#1](https://github.com/gerchowl/watcher-s1/pull/1))
