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
- Release binaries (static musl Linux x86_64/aarch64, Apple-silicon macOS) and crates.io publishing through the devkit release train

### Changed

### Deprecated

### Removed

### Fixed

### Security
