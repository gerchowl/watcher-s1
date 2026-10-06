---
type: issue
state: open
created: 2026-10-06T10:42:41Z
updated: 2026-10-06T10:43:16Z
author: gerchowl
author_url: https://github.com/gerchowl
url: https://github.com/gerchowl/watcher-s1/issues/10
comments: 1
labels: none
assignees: none
milestone: none
projects: none
parent: none
children: none
synced: 2026-10-06T15:23:29.516Z
---

# [Issue 10]: [Release 0.1.0: frozen CHANGELOG ends with a blank line](https://github.com/gerchowl/watcher-s1/issues/10)

prepare-release's changelog freeze dropped the empty trailing sections (Removed, Security) and left `\n\n` at EOF, so `end-of-file-fixer` fails CI on the release PR #8 and blocks `release.yml` final. Fix on `release/0.1.0`; upstream report to follow.
---

# [Comment #1]() by [gerchowl]()

_Posted on October 6, 2026 at 10:43 AM_

Upstream: vig-os/devkit#1842

