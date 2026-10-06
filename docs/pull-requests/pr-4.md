---
type: pull_request
state: closed (merged)
branch: feat/mkrustproject → main
created: 2026-10-05T19:07:37Z
updated: 2026-10-05T22:19:17Z
author: gerchowl
author_url: https://github.com/gerchowl
url: https://github.com/gerchowl/watcher-s1/pull/4
comments: 0
labels: none
assignees: none
milestone: none
projects: none
merged: 2026-10-05T22:19:15Z
synced: 2026-10-06T15:23:42.050Z
---

# [PR 4](https://github.com/gerchowl/watcher-s1/pull/4) build: adopt the devkit Rust pack (mkRustProject)

Item 2b: move from the hand-rolled Rust setup to devkit's Rust pack.

- **Toolchain:** `rust-toolchain.toml` pins 1.95.0 (minimal profile + rustfmt, clippy, rust-src, rust-analyzer). fenix resolves it via `toolchainHash`.
- **`checks`** come from `mkRustProject`: clippy (deny warnings), fmt, **nextest**, doctest, doc (deny warnings). The whole suite passes in the Nix sandbox, including the PTY, signal and process-group tests. `procps` is added for `ps`, and `event.schema.json` via `extraSrcFiles`.
- **`packages.default`** is the pack's `cargo auditable` build (3.1 MB). `packages.watcher-s1` aliases it.
- **Dev shell** stays `mkProjectShell` with the `.vig-os` knobs forwarded, fed the same toolchain through the `rust` capability module. The pack's own `devShell` drops the knobs (vig-os/devkit#1810), which would silently re-require `Refs:` locally.
- **CI:** the extension job runs `nix flake check -L` on Linux and macOS. It replaces the separate macOS `cargo test`, since nextest covers it. Locally: `just flake-check`.

Tracked upstream in vig-os/devkit#1833.


---
---

## Commits

### Commit 1: [6b0e9ea](https://github.com/gerchowl/watcher-s1/commit/6b0e9eabe7daa7acb1ad0acd8df98ab675aa0503) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 07:07 PM
build: adopt the devkit Rust pack (mkRustProject), 97 files modified (.github/workflows/ci-extension.yml, flake.nix, justfile.project, rust-toolchain.toml)

### Commit 2: [40d69f3](https://github.com/gerchowl/watcher-s1/commit/40d69f31cfb6c1119de5f833e8f3591c676e1090) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 07:07 PM
docs: describe the Rust pack setup and just flake-check, 11 files modified (README.md)
