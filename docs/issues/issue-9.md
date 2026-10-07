---
type: issue
state: closed
created: 2026-10-06T10:34:41Z
updated: 2026-10-06T15:47:22Z
author: watcher-s1-release[bot]
author_url: https://github.com/watcher-s1-release[bot]
url: https://github.com/gerchowl/watcher-s1/issues/9
comments: 1
labels: bug
assignees: none
milestone: none
projects: none
parent: none
children: none
synced: 2026-10-07T08:27:23.372Z
---

# [Issue 9]: [Release 0.1.0 failed — automatic rollback](https://github.com/gerchowl/watcher-s1/issues/9)

Release 0.1.0 failed during the automated release workflow.

**Workflow Run:** [View logs](https://github.com/gerchowl/watcher-s1/actions/runs/37450313508)
**Release PR:** #8

**Automatic rollback attempted:**
- Release branch: this run's finalize commit(s) reverted, but only when the branch tip matched exactly what the run wrote — otherwise the branch is left untouched and the rollback step fails loudly instead (vig-os/devkit#1462)

**Tag status (forward-fix policy):**
- Release tags are not deleted by automation (workflow choice; GitHub immutable-release lock-in applies only after a release is **published** when that setting is enabled). If a tag was pushed before the failure, it remains on the remote.
- Use a new release candidate to validate fixes, then re-run the final release when ready.
- If a draft GitHub Release exists, manage it from the Releases UI; **publishing** locks the linked tag and assets when **immutable releases** are enabled.
---

# [Comment #1]() by [gerchowl]()

_Posted on October 6, 2026 at 03:47 PM_

Superseded: v0.1.0 shipped (https://github.com/gerchowl/watcher-s1/releases/tag/v0.1.0) after #11 and #15 fixed the causes.

