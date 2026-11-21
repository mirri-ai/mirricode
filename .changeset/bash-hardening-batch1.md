---
"@mirri-ai/release": minor
---

Harden the Bash tool: command-level permissions, credential hygiene, safer output handling, and running-task observability.

- Bash permission rules and session approvals now match the exact command text (facets like `Bash(git status)`) instead of every Bash call, so a previously approved command no longer blanket-approves different commands. Legacy bare `Bash` rules keep working.
- An explicit `cwd` outside the workspace sandbox — or an in-command `cd` into one — now asks for approval before running, even when the command itself was previously approved.
- Environment variables whose names look like credentials (matching KEY, TOKEN, SECRET or PASSWORD, case-insensitive) are filtered out of Bash command environments. Add a name to the new `[bash] env_allowlist` config key if a build genuinely needs one.
- Foreground command output now interleaves stdout and stderr in arrival order, keeps only the last 1 MiB in memory, and discloses how many bytes were dropped from the start when output exceeds the cap.
- Task tools can now page through background output (`offset_bytes` / `max_bytes`) and report how long a task has been running and how long since it last produced output.
- PowerShell commands keep their current output handling — output order stays as-is with no interleaving and no in-memory output cap; user-typed `!` shell commands keep their full environment.
