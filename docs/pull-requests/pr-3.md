---
type: pull_request
state: closed (merged)
branch: feat/hardening-round2 → main
created: 2026-10-05T19:02:06Z
updated: 2026-10-05T22:06:25Z
author: gerchowl
author_url: https://github.com/gerchowl
url: https://github.com/gerchowl/watcher-s1/pull/3
comments: 0
labels: none
assignees: none
milestone: none
projects: none
merged: 2026-10-05T22:06:25Z
synced: 2026-10-06T15:23:44.374Z
---

# [PR 3](https://github.com/gerchowl/watcher-s1/pull/3) feat: writer thread, kill-before-reap, thread-level D state, --on-prompt, --log, https

Items 3–6 of the post-v1 list:

- **Stdout can't stall the watch (3).** Output goes through a writer thread and a bounded queue (64 chunks). A stalled reader back-pressures the child, but `--silence`, `--timeout` and prompt cancel keep running. Test: nobody reads stdout, `--timeout 1s` still fires within 5 s, and every byte arrives once the reader resumes.
- **Kill before reap (4).** The child's exit is observed with `waitid(WNOWAIT)`. When we killed the group (timeout, prompt cancel), the final SIGKILL goes out while the zombie leader still pins the pgid, then we reap. This closes the pid-reuse window.
- **Real wedge test (5).** A `vfork` helper puts a real process in kernel D state. Linux asserts `stalled/blocked` with a `D` process end to end. The sampler now checks **every thread** (`/proc/<pid>/task/*`), since wedges usually sit in a worker thread. Darwin has a twin test expecting `U`; CI will tell whether darwin reports a vfork parent that way.
- **`--on-prompt cancel` (6).** Sends SIGINT after `--prompt-cancel-after`, then TERM and KILL. It never answers. Final event `reason: prompt_cancelled`, a new value in the schema enum. The default stays `wait` (report only).
- **`--log FILE` passive mode (6).** Follows a growing file across truncation and rotation, never tees, and leaves by the terminating signal.
- **https (6).** rustls with the ring provider (no aws-lc/cmake), bundled Mozilla roots plus `SSL_CERT_FILE`. The deadline covers the handshake. Tests: trusted private CA works, untrusted CA fails open, a hung handshake respects the deadline.

Also: rustls's `logging` feature is off (no `log` dependency).


---
---

## Commits

### Commit 1: [1fecd7b](https://github.com/gerchowl/watcher-s1/commit/1fecd7b50b0666cffd7358cee2a87f15b31068f4) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 07:01 PM
feat: writer thread, kill-before-reap, thread-level D state, --on-prompt, --log, https, 1357 files modified (Cargo.lock, Cargo.toml, README.md, event.schema.json, src/cli.rs, src/http.rs, src/main.rs, src/probe.rs, src/supervise.rs, tests/s1.rs, tests/wrap.rs)

### Commit 2: [006e222](https://github.com/gerchowl/watcher-s1/commit/006e2229d7e4b2c610e7f34ad7d3f5c8bbfb9809) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 07:08 PM
test: declare vfork directly (the libc crate omits it on darwin), 8 files modified (tests/wrap.rs)

### Commit 3: [83c3cea](https://github.com/gerchowl/watcher-s1/commit/83c3cea4d36ef22fe31ce5566cfa7265a288c2e9) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 09:25 PM
fix: bound every TLS read by the deadline and never drop the output tail, 176 files modified (src/http.rs, src/supervise.rs, tests/s1.rs, tests/wrap.rs)

### Commit 4: [5fb476c](https://github.com/gerchowl/watcher-s1/commit/5fb476cf656ea69144306bbc9090358e099b8f98) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 09:26 PM
fix: cap the exit drain at 3 s so a forever-writing grandchild cannot hold us, 37 files modified (src/supervise.rs, tests/wrap.rs)

### Commit 5: [a797811](https://github.com/gerchowl/watcher-s1/commit/a7978110129c85784cc58d9f4a1a017f38bba796) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 09:35 PM
fix: bound the exit drain under a slow reader, honour signals in the final flush, 115 files modified (src/supervise.rs, tests/wrap.rs)

### Commit 6: [a0ae370](https://github.com/gerchowl/watcher-s1/commit/a0ae3703026725f7930f46bc2959d7f930b4f718) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 09:37 PM
test: gate the real Kev checks on WATCHER_S1_KEV_URL, not the host-wide SYSTEMONE_URL, 22 files modified (README.md, justfile.project, tests/kev.rs, tests/wrap.rs)

### Commit 7: [73407ff](https://github.com/gerchowl/watcher-s1/commit/73407ffd7fd77d36f8ef26b1e116080c1a1b22ec) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 09:50 PM
fix: apply the drain cap only while the child's group still lives, 176 files modified (src/supervise.rs, tests/wrap.rs)

### Commit 8: [4a63fe4](https://github.com/gerchowl/watcher-s1/commit/4a63fe4908f61b613a7da1fb7984335b0da907e5) by [gerchowl](https://github.com/gerchowl) on October 5, 2026 at 09:58 PM
test: drop the darwin U-state twin (macOS vfork does not park the parent), 41 files modified (tests/wrap.rs)
