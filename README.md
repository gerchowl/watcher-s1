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
| 2 | a [System One](#system-one) judgement of the last 4 KB (ten questions, one logistic score) | **only at event time**: once at exit (any exit that produced output), and at the silence threshold. Never on a poll. |

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
- **Fail open.** If System One is not configured, down or slow, the event
  carries `"s1": null` and everything else works.

## Install

As a flake input (what g-fleet does):

```nix
inputs.watcher-s1.url = "github:gerchowl/watcher-s1";
# packages.${system}.default (x86_64-linux, aarch64-linux, aarch64-darwin, x86_64-darwin)
environment.systemPackages = [ inputs.watcher-s1.packages.${system}.default ];
```

Pin a release with `github:gerchowl/watcher-s1?ref=v0.1.0`.

Without Nix:
- **Release binaries:** every GitHub Release carries
  `watcher-s1-<tag>-<target>.tar.gz` plus `.sha256`, for
  `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` (fully static,
  any distribution) and `aarch64-apple-darwin`.
- **From source:** `cargo install --git https://github.com/gerchowl/watcher-s1 --tag v0.1.0`.
- **Ad hoc:** `nix run github:gerchowl/watcher-s1 -- -- make test`.

## Usage

```text
watcher-s1 [OPTIONS] -- CMD [ARGS...]
watcher-s1 config [--s1-url URL] [--s1-timeout SECS] [--config FILE]
watcher-s1 judge --posttooluse [--s1-url URL] ...   # Claude Code hook, see below
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
| `done` | `exit` | info | exit 0 and nothing flags it (final event) |
| `failing` | `masked_failure` | warn | exit 0, but System One's score ≥ the wrapper threshold (final event) |
| `failing` | `exit` / `signal` / `timeout` | error | non-zero exit, death by signal, or killed by `--timeout` (final event) |
| `failing` | `prompt_cancelled` | error | `--on-prompt cancel` cancelled an unanswered prompt (final event) |

Every run ends with exactly one final event (`exit` non-null).

- `run_id` = `<host>:<watcher pid>:<start ms>`. It is exported to the child as
  `WATCHER_S1_PARENT`, and a nested watcher reports it as `caused_by`, so
  `watcher-s1 → g-fleet-roll → watcher-s1 → nix build` chains into one causal
  thread instead of three unrelated alerts.
- `dedup_key` = `watcher-s1:<host>:<state>:<fnv1a64(cmd)>`, stable across runs
  of the same command.

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

Releases go through the [vig-os/devkit](https://github.com/vig-os/devkit)
release train ([`docs/DOWNSTREAM_RELEASE.md`](docs/DOWNSTREAM_RELEASE.md)),
with tags `vX.Y.Z`:

```bash
gh workflow run prepare-release.yml -f version=X.Y.Z          # cut release/X.Y.Z from main
gh workflow run release.yml --ref release/X.Y.Z -f version=X.Y.Z -f release-kind=final -f dry-run=false
gh workflow run promote-release.yml --ref release/X.Y.Z -f version=X.Y.Z
```

`release.yml` tags the release and creates a **draft** GitHub Release.
`release-binaries.yml` builds the binaries into that draft, and
`promote-release.yml` publishes it and merges the release branch back to
`main`. Publishing fires `publish-release-extension.yml`, which stays
devkit's no-op: the crate is not published to crates.io. The train needs the
`COMMIT_APP_*` and `RELEASE_APP_*` GitHub App secrets, from the
`watcher-s1-commit` and `watcher-s1-release` Apps. `*_CLIENT_ID` holds the
numeric App id, which GitHub accepts as the token issuer in place of the
client ID. `sync-issues` stays enabled because devkit's release finalize
dispatches it (vig-os/devkit#1843).

## License

Apache-2.0, see [LICENSE](LICENSE).
