---
type: issue
state: open
created: 2026-10-07T15:17:23Z
updated: 2026-10-07T15:17:23Z
author: gerchowl
author_url: https://github.com/gerchowl
url: https://github.com/gerchowl/watcher-s1/issues/31
comments: 0
labels: none
assignees: none
milestone: none
projects: none
parent: none
children: none
synced: 2026-10-07T15:39:28.620Z
---

# [Issue 31]: [0.2.0 final release: run stopped by a GitHub internal server error after finalize](https://github.com/gerchowl/watcher-s1/issues/31)

release.yml run 37641498469: `core` (validate, finalize, test) succeeded, then GitHub failed the workflow with **Internal server error** (correlation ID 31ff2658-aac4-4ec2-a157-75ba09bbba32) before creating the `extension`, `publish` and `rollback` jobs. So no tag, no draft and no automatic rollback; `release/0.2.0` still carries the finalize commit 515260d (date stamp), which makes a re-dispatch fail validate's TBD check.

Recovery per devkit's RELEASE_CYCLE.md (manual rollback): revert the finalize commit through a PR into `release/0.2.0` (the release ruleset forbids force-push), then re-dispatch `release.yml` final.
