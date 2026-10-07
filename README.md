<!-- Seeded by vigOS devkit — yours to edit; upgrades never overwrite this file. -->
<!-- Bugs / missing tools: https://github.com/vig-os/devkit/issues -->

# watcher-s1

Run a command and watch it. `watcher-s1` wraps one command, passes its output
through unchanged, and reports what it sees — **stalled**, **waiting on
input**, **failing**, **done** — as JSON events on a sideband. The exit status
is always the child's own.

```console
$ watcher-s1 --silence 5m --timeout 2h -- nix build .#foo
```

Design and measurements: [g-fleet#244](https://github.com/gerchowl/g-fleet/issues/244).

## What it is, and what it isn't

**It is** a truthful wrapper with three detection tiers:

| Tier | What | When it runs |
|---|---|---|
| 0 | output-silence timer, hard `--timeout` (TERM then KILL the whole process group), process-state sampler (Linux `D` state + `wchan`, darwin `U` state) | continuously, cheap |
| 1 | regex on the unterminated last line for prompts (`password:`, `[y/N]`, `(yes/no)`, host-key questions, "Press … key", …); a weak error-line panel | when the output goes quiet |
| 2 | a [System One](#system-one) judgement of the last 4 KB (ten questions, one logistic score) | **only at event time**: once at exit (any exit that produced output), and at the silence threshold. Never on a poll, with one opt-in exception: `--heartbeat-s1` asks once per heartbeat (subject to the in-flight guard and the breaker, see [Heartbeats](#heartbeats)). |

**It is not** an alerting system. It owns no policy: no Telegram, no quiet
hours, no dedupe windows. It emits events with a `severity` and a stable
`dedup_key`; a separate gateway
([g-fleet#188](https://github.com/gerchowl/g-fleet/issues/188)) decides what
reaches a human. It also doesn't attach to running processes (`--pid`): v1
only wraps (`watcher-s1 -- cmd`) or passively follows a log file
(`--log FILE`), and it never answers a prompt for you: at most it cancels one
(`--on-prompt cancel`).

### Guarantees

- **Truthful exit.** Exit code `n` → `watcher-s1` exits `n`. Death by signal
  → the same signal is re-raised with its default action, so a shell sees
  `128+n` (`143` for SIGTERM, `137` for SIGKILL). The verdict never changes the
  exit code.
- **Output unchanged.** Bytes are teed through as they arrive. Under the PTY
  (the default) the child's `\n` stays `\n`; there is no CRLF rewriting.
- **Own process group.** The child leads its own group (under the PTY, its own
  session with the PTY as controlling terminal). `--timeout` and forwarded
  signals (`INT`, `TERM`, `HUP`, `QUIT`, `USR1`, `USR2`) reach the whole
  group, grandchildren included.
- **PTY by default.** Programs show prompts and progress only on a TTY, and
  pipe buffering would trip the silence timer falsely. Prompts written to
  `/dev/tty` (sudo, ssh) are seen as well. `--pipe` opts out (stdout and
  stderr stay separate). A non-TTY stdin is passed straight through, so
  `watcher-s1 -- wc -l < file` works; a TTY stdin is forwarded in raw mode.
- **Bounded probes.** Every external probe (process sampling, DNS, HTTP to
  System One) runs under a wall-clock timeout. A probe that times out counts as
  "state unknown, maybe wedged". `/nix/store` is never stat'ed.
- **Your stdout never stalls the watch.** Output goes to stdout through a
  writer thread and a bounded queue. If whoever reads it stops, the child is
  back-pressured but the timers (`--silence`, `--timeout`, prompt cancel) keep
  running. Like any process, watcher-s1 still finishes its last write before
  it exits, so it waits for a reader that comes back. At exit it drains what
  the child left in the pipes (at most 3 s if a grandchild keeps writing).
- **A stalled event reader never stalls the watch.** Events leave through a
  writer thread and a bounded queue (256), so a full `--events-fd` pipe or
  stderr cannot freeze `--timeout`, signal forwarding or heartbeats. When the
  queue is full: heartbeats are dropped (the next one that gets through
  carries `heartbeats_dropped`), other events wait in a bounded overflow
  queue (1024, oldest dropped with a diagnostic unless `-q`), and event
  order is kept. The watcher's own `watcher-s1 (log):` diagnostics and events
  on the default stderr sink share one stderr writer thread behind a bounded
  queue: a full queue drops (and counts) diagnostics instead of blocking, so a
  stalled stderr cannot hold up `--timeout` either. The final event is written after everything queued; like any
  process writing to a full pipe, watcher-s1 may block at exit on an event
  sink nobody reads.
- **Fail open.** If System One is not configured, down or slow, the event
  carries `"s1": null` and everything else works.

## Install

As a flake input (what g-fleet does):

```nix
inputs.watcher-s1.url = "github:gerchowl/watcher-s1";
# packages.${system}.default (x86_64-linux, aarch64-linux, aarch64-darwin, x86_64-darwin)
environment.systemPackages = [ inputs.watcher-s1.packages.${system}.default ];
```

Pin a release with `github:gerchowl/watcher-s1?ref=v0.2.0`.

Without Nix:
- **Release binaries:** every GitHub Release carries
  `watcher-s1-<tag>-<target>.tar.gz` plus `.sha256`, for
  `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` (fully static,
  any distribution) and `aarch64-apple-darwin`.
- **From source:** `cargo install --git https://github.com/gerchowl/watcher-s1 --tag v0.2.0`.
- **Ad hoc:** `nix run github:gerchowl/watcher-s1 -- -- make test`.

## Usage

```text
watcher-s1 [OPTIONS] -- CMD [ARGS...]
watcher-s1 config [--s1-url URL] [--s1-timeout SECS] [--config FILE]
watcher-s1 judge --posttooluse [--s1-url URL] ...   # Claude Code hook, see below
watcher-s1 follow [--new] [--timeout DUR] EVENTS_FILE  # stream events until the run ends (exit codes: `watcher-s1 guide`)
watcher-s1 guide                                    # print the agent guide
watcher-s1 mcp [--channel] [--state-dir DIR]        # MCP server on stdio, see "MCP server" below
```

| Option | Default | Meaning |
|---|---|---|
| `--pipe` | off | plain pipes instead of a PTY |
| `--silence DUR` | `300s` | output-silence threshold (`0` disables) |
| `--timeout DUR` | none | hard limit: SIGTERM the process group, SIGKILL after `--kill-grace` |
| `--kill-grace DUR` | `10s` | TERM → KILL grace |
| `--prompt-after DUR` | `5s` | quiet time before a prompt-shaped last line counts as `waiting_on_input` |
| `--sample-every DUR` | `10s` | process-state sampling interval while quiet |
| `--blocked-after DUR` | `60s` | a tree blocked in `D`/`U` (or unprobeable) this long raises `stalled` |
| `--probe-timeout DUR` | `2s` | wall-clock limit for one process-state probe |
| `--on-prompt wait\|cancel` | `wait` | `cancel`: an unanswered prompt gets SIGINT after `--prompt-cancel-after`, then TERM and KILL with `--kill-grace` between (final event `reason: prompt_cancelled`) |
| `--prompt-cancel-after DUR` | `60s` | how long a prompt may wait before `cancel` acts |
| `--heartbeat DUR` | off | emit a `heartbeat` status event every DUR (minimum `1s`), see [Heartbeats](#heartbeats) |
| `--heartbeat-s1` | off | attach a System One verdict to each heartbeat (needs `--heartbeat`; at most one request in flight, so a slow endpoint gets fewer calls than ticks, and the breaker applies) |
| `--log FILE` | — | passive mode, see below (instead of `-- CMD`) |
| `--evidence-bytes N` | `1500` | output tail carried in each event |
| `--events FILE` | — | append events as JSON lines to FILE |
| `--events-fd N` | — | write events as JSON lines to an inherited fd |
| *(neither)* | stderr | one line per event, prefixed `watcher-s1: ` |
| `-q, --quiet` | off | drop the `watcher-s1 (log): …` diagnostics (events still flow) |
| `--s1-url URL`, `--s1-timeout SECS`, `--config FILE`, `--no-s1` | — | System One, see below |

### Passive mode: `--log FILE`

`watcher-s1 --log /var/log/job.log --silence 10m` follows a growing file
instead of running anything. Silence, prompts and the System One silence
judgement work as in wrap mode; there is no process to sample, no exit and
no final event, and nothing is teed. The file is followed across truncation
and rotation (a new file at the same path). It runs until SIGINT/SIGTERM/
SIGHUP/SIGQUIT and leaves by that signal. Events carry `pid: 0` and
`cmd: "--log <path>"`.

Durations take `250ms`, `30s`, `5m`, `2h` or bare seconds. `watcher-s1`'s own
usage errors exit `2`; a command that cannot be found exits `127` (not
runnable: `126`), like a shell.

## Events

One JSON object per event, versioned by [`event.schema.json`](event.schema.json)
(`schema: 1`; new optional fields may appear within schema 1, so consumers
must ignore unknown fields).

```json
{
  "schema": 1, "ts": "2026-10-05T13:31:15.203Z", "host": "vm-dev", "source": "watcher-s1",
  "run_id": "vm-dev:1145652:1791207075201",
  "cmd": "cargo build", "pid": 1145653, "pgid": 1145653,
  "state": "failing", "reason": "masked_failure",
  "exit": {"code": 0, "signal": null},
  "severity": "warn",
  "dedup_key": "watcher-s1:vm-dev:failing:69a0ea2cbd2e4598",
  "caused_by": null,
  "evidence_tail": "error: could not compile `foo` (bin \"foo\") due to 1 previous error\n",
  "s1": {"endpoint": "http://kev:8023/v1/systemone", "failing": 0.78, "clean_done": 0.06, "fused": 0.86, "latency_ms": 214}
}
```

| `state` | `reason` | `severity` | Emitted when |
|---|---|---|---|
| `stalled` | `silence` | warn | no output for `--silence` |
| `stalled` | `blocked` | warn | a process (or any of its threads) sat in `D`/`U` state, or the probe hung, for `--blocked-after` (adds `proc`) |
| `waiting_on_input` | `prompt` | warn | the last line is an unanswered prompt (adds `prompt`) |
| `failing` | `silence` | warn | silence, and System One reads the tail as an unrecovered failure |
| `progressing` | `resumed` | info | output resumed after a warn event |
| *current* | `heartbeat` | info | every `--heartbeat` interval (adds `elapsed_ms`, `bytes_since_last`, `lines_since_last`, `last_line`, and `heartbeats_dropped` after drops) |
| `done` | `exit` | info | exit 0 and nothing flags it (final event) |
| `failing` | `masked_failure` | warn | exit 0, but System One's score ≥ the wrapper threshold (final event) |
| `failing` | `exit` / `signal` / `timeout` | error | non-zero exit, death by signal, or killed by `--timeout` (final event) |
| `failing` | `prompt_cancelled` | error | `--on-prompt cancel` cancelled an unanswered prompt (final event) |
| `failing` | `stopped` | error | the run was stopped through its control socket, as the MCP `watch_stop` does: TERM to the group, KILL after the grace, the job's real signal exit (final event) |

Every run ends with exactly one final event (`exit` non-null).

### Heartbeats

Events are edge-triggered, so a healthy job that runs for an hour emits
nothing until it ends. `--heartbeat DUR` (default off, minimum `1s`) adds a
periodic `reason: heartbeat` event so a reader can tell "running fine" from
"the watcher died". Pair it with `--events FILE`: heartbeats go to the same
sink as every other event, so with no `--events`/`--events-fd` they land on
stderr, interleaved with the child's output under `--pipe`.

- `state` is the current episode state (`stalled` or `waiting_on_input` while
  one is open, otherwise `progressing`); `severity` is always `info`, since the
  edge event already alerted. Because a heartbeat repeats the open episode's
  state, a `progressing` heartbeat means no episode is open, not that output
  resumed.
- `dedup_key` is `watcher-s1:<host>:heartbeat:<hash>`: all of a job's
  heartbeats fold into one key whatever their state.
- Extra fields: `elapsed_ms` (since start, since attach in `--log` mode),
  `bytes_since_last` and `lines_since_last` (child output since the previous
  heartbeat), `last_line` (last non-empty line, ANSI stripped, at most 200
  characters, `null` if none; tracked over the whole output, not just the
  evidence tail), plus the usual `evidence_tail`. `heartbeats_dropped` appears
  only after heartbeats were dropped because the event sink was not keeping up:
  it counts those since the previous delivered one. Heartbeats are
  informational, so under a stalled sink they are shed rather than queued.
- Ticks sit at `start + k·DUR` on the monotonic clock. If the watcher was
  blocked past several ticks it emits one heartbeat, not a burst. A heartbeat
  is our output, not the child's: it never resets `--silence` or triggers
  `resumed`.
- No System One call by default (tier 2 is event-time only). `--heartbeat-s1`
  makes each heartbeat ask for a verdict, through the same breaker, deadline
  and fail-open path as the silence-time call; it is attached as `s1` and never
  changes `state`. Liveness comes first: at most one such request is in
  flight. If it is still pending when the next tick is due, that heartbeat
  is emitted with `s1: null` and so is every later tick's, with no new
  request until that worker returns (its late verdict is discarded) or dies;
  a worker that died fails open the same way. At exit (or on a signal in
  `--log` mode) a pending verdict is waited for at most 250 ms, then the
  heartbeat goes out with `s1: null`, so heartbeats precede the final event
  but never hold it up.
- `--log` mode emits heartbeats too.

Heartbeats add `heartbeat` to the `reason` enum within schema 1. They appear
only when you opt in, but a consumer validating strictly against the old enum
would reject them.

- `run_id` = `<host>:<watcher pid>:<start ms>`. It is exported to the child as
  `WATCHER_S1_PARENT`, and a nested watcher reports it as `caused_by`, so
  `watcher-s1 → g-fleet-roll → watcher-s1 → nix build` chains into one causal
  thread instead of three unrelated alerts.
- `dedup_key` = `watcher-s1:<host>:<state>:<fnv1a64(cmd)>`, stable across runs
  of the same command. Heartbeats use the literal segment `heartbeat` in place
  of the state.

## System One

Tier 2 asks a System One endpoint (Kev's `/v1/systemone` typed-question
contract) to judge the last 4 KB of output. **No endpoint is compiled in.**
Each setting resolves independently, highest first:

1. CLI: `--s1-url URL` (repeatable, ordered), `--s1-timeout SECS`,
   `--config FILE` (replaces the file search), `--no-s1`
2. env `SYSTEMONE_URL` (deliberately generic; other clients such as goal-s1
   read it too; comma or space separated)
3. the first existing file of `$XDG_CONFIG_HOME/watcher-s1/config.toml`
   (default `~/.config/…`), then `/etc/watcher-s1/config.toml`
4. none: the tier is off (logged once), everything else works

```toml
[systemone]
urls      = ["http://host:8023/v1/systemone"]  # ordered; the first healthy one wins
timeout_s = 3.0                                 # per call
breaker   = { fails = 3, cooldown_s = 600 }     # per endpoint
questions = "builtin"                           # or a path to a question file
```

`watcher-s1 config` prints the resolved values and where each came from.

- **One request per state** carries every question (the server caches the
  state prefix). The state is
  `Command: <cmd>\n(The exit status is not shown.)\nLast output:\n<last 4 KB>`.
- **Circuit breaker per endpoint**, persisted in
  `$XDG_STATE_HOME/watcher-s1/breaker.json` (`WATCHER_S1_STATE_DIR`
  overrides), so it holds across processes: after `fails` consecutive
  failures an endpoint is skipped for `cooldown_s`, then one call is let
  through.
- The host is resolved once per process, under the deadline. `http://` and
  `https://` are supported; TLS is rustls, trusting the bundled Mozilla roots
  plus any PEM bundle in `SSL_CERT_FILE` (for a private CA).

### Questions

The built-in set is data: [`questions/builtin.toml`](questions/builtin.toml),
embedded in the binary. It holds ten typed questions about the output:
- `noul` phrasings of "did it fail?", for example "Would this command most
  likely exit with a non-zero status?" and "Does the output end with an error
  message?";
- an `outcome` choice (success / failure / partial / info).

They combine into one logistic score (`s1.fused` in events). Its weights were
fitted and measured on 1,474 real labelled commands: AUC 0.95, against 0.88
for the previous two-question default, which flagged only 18 % of real
failures. Method and tables: [`docs/eval/questions-spike.md`](docs/eval/questions-spike.md).

**Thresholds per surface:**
- **The wrapper flags at ≥ 0.5:** recall 0.81, false-positive rate 4 %. It
  judges rare events.
- **The PostToolUse judge flags at ≥ 0.8:** recall 0.71, false-positive rate
  2.2 %. It fires on every piped call, so it is stricter.

To change the questions, the score or the thresholds without rebuilding,
point `questions = "/path/q.toml"` (or `.json`) at a file of the same shape:
- `[score] kind = "logistic"` takes a `bias` plus `weights`, keyed by a
  `noul` name or `"<choice>:<label>+<label>"`;
- `kind = "mean"` takes `positive` / `negative`;
- `threshold` is a number, or `{ wrap, judge }`.

Re-run [`eval/`](eval) before changing any wording: a reworded question
answers differently.

## Claude Code

Agents: `watcher-s1 guide` prints a short how-to ([docs/agent-guide.md](docs/agent-guide.md)),
and `watcher-s1 follow EVENTS_FILE` streams the events of a backgrounded run.

Run long commands under the watcher with `run_in_background: true`. The Bash
call returns immediately, and the agent is woken when the command **exits**,
with the true exit code. Meanwhile the events tell it what is going on:

```bash
watcher-s1 --silence 10m --timeout 2h --events /tmp/build.events -- nix build .#foo
```

- Exit → the agent wakes and reads the exit code (truthful) plus the final
  event (`masked_failure` catches an exit 0 that hides a failure).
- Mid-run, the agent (or a Monitor) can `tail` the events file for
  `waiting_on_input` (a prompt nobody will answer: kill it and re-run
  non-interactively) or `stalled` (silence / a `D`-state wedge).
- Without `--events`, events land on stderr as `watcher-s1: {…}` lines, which
  the background task's output file captures alongside the command's output.

### MCP server: `watcher-s1 mcp`

For agents that speak MCP (Claude Code, Codex, ...), `watcher-s1 mcp` serves
the supervisor over stdio, so an agent starts a job, goes on working, and
asks for the verdict, with no shell juggling. It is part of the default
build (cargo feature `mcp`; `cargo build --no-default-features` leaves it out
and the subcommand then exits 2).

Register it once:

```bash
claude mcp add watcher-s1 -- watcher-s1 mcp
```

| Tool | Does |
|---|---|
| `watch_start {cmd, cwd?, silence?, timeout?, heartbeat?, s1?}` | runs `cmd` (an argv array) as a fully wrapped `watcher-s1 --pipe --events ... --control ... -- cmd`, detached into its own session so it outlives the server and the session; returns the run `id` and the events and log paths. The run is recorded (`starting`) before anything is launched; if the launch fails it is marked `failed` and nothing runs |
| `watch_wait {id, until, timeout_s?}` | blocks until the next unseen event (`until: "next"`) or the final one (`"final"`), or `timeout_s` (default 60); returns the events as JSON plus the compact `follow` lines. At most 100 events and 256 KiB per call, with `more: true` when there is more to fetch. Works for runs an earlier server started |
| `watch_status {id}` | last event, event count, elapsed time, and `state`: `running`, `finished`, `lost` (the watcher is gone and left no final event), `starting` or `failed` |
| `watch_stop {id, grace_s?}` | asks the run's supervisor to stop the job: TERM to its whole process group, SIGKILL to the group after `grace_s` (default 5) and again just before the leader is reaped, so a descendant that ignores TERM dies even when the leader exits at once. The final event has `reason: stopped` and the job's real signal exit |
| `watch_list {limit?}` | runs, newest first, with state |

The resource `watcher-s1://guide` is the same text as `watcher-s1 guide`.

Runs live under `--state-dir` (default `$WATCHER_S1_STATE_DIR`, else
`$XDG_STATE_HOME/watcher-s1`, else `~/.local/state/watcher-s1`), in
`runs/<id>/`: `events.jsonl`, `output.log` (the job's output only),
`meta.json` (command, cwd, start time, watcher pid for information, options),
the read state (`cursor`, `summary`, `cursor.lock`) and the watcher's
`control.sock`. The directory is the source of truth: a new server sees every
run.

**Pruning.** Runs older than 7 days whose watcher no longer answers are
removed when a server starts and then every hour while it runs. A run whose
watcher is still alive is never pruned, however old. Pruning is the only
cleanup: **`output.log` is not bounded** while a job runs (nothing caps what
the job writes), and the 7-day age rule is not a disk quota. Bound a run with
`timeout`, and stop or prune what you no longer need.

**Control and trust.** The server never signals a process id. Whether a
watcher is alive, and stopping it, go through its private Unix socket
(`control.sock`, mode 0600; when the run directory's path is too long for a
socket path it lives under `$XDG_RUNTIME_DIR/watcher-s1` or
`/tmp/watcher-s1-<uid>`). The socket is served by the hidden
`--control PATH` flag of the wrapper; a socket nothing answers on means the
run is not running. The run id (the directory name) is the only thing paths
are derived from: a `meta.json` naming another id is refused, paths stored in
it are ignored, symlinked run directories and state files and non-regular
files are rejected, and state files are size-bounded. The state directory
belongs to you; none of this is a defence against the user it belongs to.

**Delivery.** `watch_wait` keeps a per-run cursor (a byte offset into
`events.jsonl`), updated under a lock that is shared by overlapping calls and
by every server process using the state directory, so two waiters never get
the same event. The cursor moves when a response is produced. A response that
never reaches the model (connection lost, call cancelled) loses its
intermediate events for that caller; `events.jsonl` keeps everything, and
`watch_status` shows the last event. The final verdict is the exception: every
later wait repeats it with `already_seen: true`, so it is delivered at least
once.

stdout carries the protocol only; diagnostics go to stderr.

#### Channels (opt-in, research preview)

With `--channel` the server declares the experimental `claude/channel`
capability and pushes one `notifications/claude/channel` message per edge
event (`stalled`, `waiting_on_input`, `failing`, and the final event, never
heartbeats) of the runs it started: the compact `follow` line as the content,
`run_id` (the id the tools take), `state` and `reason` as `<channel>` tag
attributes. Claude Code must be started with channels enabled for the server;
while channels are a research preview, a custom server needs the development
flag, which asks for confirmation:

```bash
claude mcp add watcher-s1 -- watcher-s1 mcp --channel
claude --dangerously-load-development-channels server:watcher-s1
```

Team and Enterprise organisations must enable channels in their settings
first. Without the flag the capability is simply ignored and the tools work
as usual.

### PostToolUse hook: masked pipes

`… | tail` hides the producer's exit status: the Bash tool reports the
filter's 0. In g-fleet#244's audit, about 3–10 % of agents' piped exit-0
Bash calls hid a real failure. `watcher-s1 judge --posttooluse` catches them
without wrapping anything:

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "watcher-s1", "args": ["judge", "--posttooluse"], "timeout": 5 }
        ]
      }
    ]
  }
}
```

PostToolUse only fires for successful calls, so every input is an exit 0. The
hook acts only when all of these hold: the command pipes into a filter
(`tail`, `head`, `grep`, `rg`, `sed`, `awk`, `sort`, `tee`, `jq`, …; a
`pipefail` command is skipped), the call was neither backgrounded nor
interrupted, and a System One endpoint is configured (same precedence as
above). It judges the last 4 KB of stdout and stderr. At a score ≥ the judge threshold (0.8) it
prints:

```json
{"hookSpecificOutput": {"hookEventName": "PostToolUse",
  "additionalContext": "watcher-s1: exit 0 came from the pipe; the output shows an unrecovered failure: error: could not compile `foo` … (System One fused score 0.86 >= 0.80). Re-run without the filter, or with `set -o pipefail`, before trusting this result."}}
```

Claude Code shows that next to the tool result. Otherwise it is a silent
no-op. A watchdog caps the whole hook at 2.9 s (the System One call gets
what remains), it always exits 0 and prints nothing on stderr, and any error
(bad JSON, endpoint down, breaker open) fails open. It never blocks the tool,
which has already run anyway.

## Development

This repo uses [vig-os/devkit](https://github.com/vig-os/devkit) (direnv mode,
trunk workflow, solo profile with `scanning` kept because the repo is public).
`direnv allow` (or `nix develop`) gives the Rust toolchain pinned by
`rust-toolchain.toml` and the pre-commit hooks, including `cargo fmt --check`
and `cargo clippy -D warnings`. The flake uses devkit's Rust pack
(`vigos.lib.mkRustProject`): `checks` (clippy, fmt, nextest, doctest, doc)
and the `cargo auditable` package come from it. The dev shell is
`mkProjectShell` fed the same toolchain, because the pack's own dev shell does
not forward the `.vig-os` hook settings yet (vig-os/devkit#1810).

```bash
just lint       # rustfmt check + clippy -D warnings
just test       # cargo test --locked: unit + integration (fake System One server)
just precommit  # every hook, as CI runs them
just nix-build  # the flake package
just flake-check  # the Rust pack's checks, nextest included, in the Nix sandbox
just test-kev url=http://…/v1/systemone   # opt-in real System One checks, sequential
```

The real-endpoint tests (`tests/kev.rs`) only run when the test-only
`WATCHER_S1_KEV_URL` is set (`just test-kev` sets it; a host-wide
`SYSTEMONE_URL` does not trigger them),
and they make their calls strictly one after another. The host is wedge-prone, so
never parallelise them.

## Releasing

Branches follow devkit's gitflow model: work lands on `dev` through PRs, and
`main` only takes releases. Rulesets require a PR and a green `CI Summary` on
`dev` and `main`, plus one approval on `main`; only the release Apps may write
`v*` tags. Releases go through the
[vig-os/devkit](https://github.com/vig-os/devkit) release train
([`docs/DOWNSTREAM_RELEASE.md`](docs/DOWNSTREAM_RELEASE.md)), with tags
`vX.Y.Z`:

```bash
gh workflow run prepare-release.yml -f version=X.Y.Z          # cut release/X.Y.Z from dev
gh workflow run release.yml --ref release/X.Y.Z -f version=X.Y.Z -f release-kind=final -f dry-run=false
gh workflow run promote-release.yml --ref release/X.Y.Z -f version=X.Y.Z
```

Mark the release PR ready and wait for green CI before `release.yml`; approve
it (it is opened by the Release App) right before promote. `release.yml` tags
the release and creates a **draft** GitHub Release.
`release-binaries.yml` builds the binaries into that draft, and
`promote-release.yml` publishes it and merges the release PR into `main`;
`sync-main-to-dev.yml` then opens a PR bringing `main` back into `dev`. Delete
the merged `release/X.Y.Z` branch afterwards, or the next `prepare-release`
refuses (vig-os/devkit#1849). Publishing fires `publish-release-extension.yml`, which stays
devkit's no-op: the crate is not published to crates.io. The train needs the
`COMMIT_APP_*` and `RELEASE_APP_*` GitHub App secrets, from the
`watcher-s1-commit` and `watcher-s1-release` Apps. `*_CLIENT_ID` holds the
numeric App id, which GitHub accepts as the token issuer in place of the
client ID. `sync-issues` stays enabled because devkit's release finalize
dispatches it (vig-os/devkit#1843).

## License

Apache-2.0, see [LICENSE](LICENSE).
