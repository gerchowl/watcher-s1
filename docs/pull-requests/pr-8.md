---
type: pull_request
state: closed (merged)
branch: release/0.1.0 → main
created: 2026-10-06T10:30:53Z
updated: 2026-10-06T15:39:59Z
author: watcher-s1-release[bot]
author_url: https://github.com/watcher-s1-release[bot]
url: https://github.com/gerchowl/watcher-s1/pull/8
comments: 0
labels: none
assignees: none
milestone: none
projects: none
merged: 2026-10-06T15:39:59Z
synced: 2026-10-07T08:27:30.128Z
---

# [PR 8](https://github.com/gerchowl/watcher-s1/pull/8) chore: release 0.1.0

# Release 0.1.0

This PR prepares release 0.1.0 for merge to main.

## [0.1.0] - TBD

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



---
---

## Commits

### Commit 1: [3ce9ffa](https://github.com/gerchowl/watcher-s1/commit/3ce9ffaa287491a0e85062f034cee5c730330e4f) by [watcher-s1-commit[bot]](https://github.com/apps/watcher-s1-commit) on October 6, 2026 at 10:28 AM
chore: freeze changelog for release 0.1.0, 19 files modified (CHANGELOG.md)

### Commit 2: [6275117](https://github.com/gerchowl/watcher-s1/commit/62751177c0ee08ec4a6f65039dc33ca371d73b6f) by [gerchowl](https://github.com/gerchowl) on October 6, 2026 at 10:42 AM
fix: end CHANGELOG.md with a single newline, 1 file modified (CHANGELOG.md)

### Commit 3: [70155c6](https://github.com/gerchowl/watcher-s1/commit/70155c6c74020419ab26295a1cacd9ec06ff52bf) by [watcher-s1-commit[bot]](https://github.com/apps/watcher-s1-commit) on October 6, 2026 at 11:07 AM
chore: finalize release 0.1.0, 2 files modified (CHANGELOG.md)

### Commit 4: [62a8d02](https://github.com/gerchowl/watcher-s1/commit/62a8d02b75362f716157b88885dc7a798fb83b6d) by [watcher-s1-commit[bot]](https://github.com/apps/watcher-s1-commit) on October 6, 2026 at 11:09 AM
revert: undo finalize release 0.1.0 (workflow rollback), 2 files modified (CHANGELOG.md)

### Commit 5: [b11b073](https://github.com/gerchowl/watcher-s1/commit/b11b0739313bccf88677c28e1a4a991d768cd0de) by [gerchowl](https://github.com/gerchowl) on October 6, 2026 at 02:40 PM
chore: re-enable sync-issues for the release train, 2 files modified (.vig-os)

### Commit 6: [de0b1b7](https://github.com/gerchowl/watcher-s1/commit/de0b1b78aff5e88f0788c87432ea70194cf59895) by [gerchowl](https://github.com/gerchowl) on October 6, 2026 at 02:41 PM
chore: re-render the scaffold with sync-issues, 365 files modified (.github/label-taxonomy.toml, .github/workflows/sync-issues.yml)

### Commit 7: [4f4edf1](https://github.com/gerchowl/watcher-s1/commit/4f4edf1c09689c819c66e6f6d044442a1c7c8a14) by [watcher-s1-commit[bot]](https://github.com/apps/watcher-s1-commit) on October 6, 2026 at 03:21 PM
chore: finalize release 0.1.0, 2 files modified (CHANGELOG.md)

### Commit 8: [38e69d9](https://github.com/gerchowl/watcher-s1/commit/38e69d9c3035d946801e53e16bff7e0b0b3283b3) by [watcher-s1-commit[bot]](https://github.com/apps/watcher-s1-commit) on October 6, 2026 at 03:23 PM
chore: sync issues and PRs, 667 files modified
