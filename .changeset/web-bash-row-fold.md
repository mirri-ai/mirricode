---
"@mirri-ai/release": patch
---

web: Make bash tool-call rows easier to scan: a `cd` separated by a newline folds like one joined with `&&`, trailing `# comments` drop out of the collapsed summary (the expanded card still shows the full command), and directories inside the workspace render as a relative-path badge — with `.worktrees/<name>` checkouts badged by their worktree name.
