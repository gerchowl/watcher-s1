---
type: pull_request
state: closed (merged)
branch: feat/release-train → main
created: 2026-10-05T19:11:15Z
updated: 2026-10-05T22:43:15Z
author: gerchowl
author_url: https://github.com/gerchowl
url: https://github.com/gerchowl/watcher-s1/pull/5
comments: 0
labels: none
assignees: none
milestone: none
projects: none
merged: 2026-10-05T22:43:13Z
synced: 2026-10-06T15:23:39.463Z
---

# [PR 5](https://github.com/gerchowl/watcher-s1/pull/5) chore: enable the devkit release train, release binaries and crates.io publishing

Item 7.

- **Release train back on:** `release` removed from `DEVKIT_FEATURES_DISABLED`, `DEVKIT_TAG_PREFIX=v` (the flake/Cargo convention: `?ref=v0.1.0`). The re-render restores prepare/release/promote/abandon, `CHANGELOG.md` and `docs/DOWNSTREAM_RELEASE.md`. Scaffold drift stays clean, because this is the installer's own output.
- **`release-binaries.yml`** (consumer-owned, tag push), built to devkit's pre-publish-window recipe:
  - builds static musl `x86_64`/`aarch64` Linux and `aarch64-apple-darwin`;
  - uploads `watcher-s1-<tag>-<target>.tar.gz` and `.sha256` into the train's **draft** Release;
  - never publishes it (promote does).
- **`publish-release-extension.yml`:** `cargo publish` after promote, final releases only. It's idempotent (skips a version already on crates.io), and **skips with a notice until `CARGO_REGISTRY_TOKEN` exists**. `cargo publish --dry-run` passes. The name `watcher-s1` is free on crates.io.
- **CHANGELOG** Unreleased filled for 0.1.0. The README gains a Releasing section and install-from-release instructions.

**Needs secrets before the first release (not in this PR):**
- `COMMIT_APP_CLIENT_ID` and `COMMIT_APP_PRIVATE_KEY`
- `RELEASE_APP_CLIENT_ID` and `RELEASE_APP_PRIVATE_KEY`
- `CARGO_REGISTRY_TOKEN` (optional; crates.io stays skipped without it)


---
---

## Commits

### Commit 1: [f47038a](https://github.com/gerchowl/watcher-s1/commit/f47038af4203dd23d2bd143a5b2eef91f478a1d2) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 07:09 PM
chore: re-enable the devkit release train (tag prefix v), 4 files modified (.vig-os)

### Commit 2: [cbaa2ae](https://github.com/gerchowl/watcher-s1/commit/cbaa2ae402a6e4df1c640e49cf1a8ac412df54ce) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 07:09 PM
chore: re-render the scaffold with the release train, 3904 files modified

### Commit 3: [993fe0d](https://github.com/gerchowl/watcher-s1/commit/993fe0d9dcc219a4dc7c7e8a2a859c8ae4e23d2f) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 07:11 PM
ci: release binaries into the draft Release and publish to crates.io, 189 files modified (.github/workflows/publish-release-extension.yml, .github/workflows/release-binaries.yml, CHANGELOG.md, Cargo.toml, README.md)

### Commit 4: [9dfb26b](https://github.com/gerchowl/watcher-s1/commit/9dfb26b165827f6403b13325f59eddaf005ca23b) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 10:32 PM
docs: changelog for #3, #4 and #6, 10 files modified (CHANGELOG.md)
