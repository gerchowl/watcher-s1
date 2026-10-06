# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

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

### Deprecated

### Removed

### Fixed

- The final group SIGKILL after a timeout or prompt cancel is sent before reaping, so it cannot hit a reused pid ([#3](https://github.com/gerchowl/watcher-s1/pull/3))
- Interactive runs: stdin is read only when ready, output keeps CR/LF on the terminal, an inherited `SIG_IGN` (nohup) is respected ([#1](https://github.com/gerchowl/watcher-s1/pull/1))

### Security
