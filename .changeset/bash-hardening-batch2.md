---
"@mirri-ai/release": minor
---

Harden background command handling: live output streaming, silence warnings, and approvals for protected home directories.

- Background commands now stream their output to the task list as it is produced, instead of only becoming visible when the command finishes. User-typed `!` shell commands carry their accumulated output into the task row when they are handed to the background.
- When a background process task goes silent for longer than the `[task]` `silenceWarnSecs` threshold in `config.toml` (default 300 seconds, `0` disables), the agent is warned once per silent stretch with the task id, its description, how long it has been silent, the tail of its output, and suggested next actions; the warning re-arms when output resumes. A session restored after a restart derives the silence baseline from the log's last-write time.
- Commands that touch protected home directories — `~/Desktop`, `~/Documents`, `~/Downloads`, matched case-insensitively as the first path segment — now ask for approval before running. A grant covers the rest of the session, and sandbox verdicts take precedence over this check. As part of this change, `~`-prefixed directory switches that previously ran without any check now trigger a conservative approval prompt.
- Paged TaskOutput responses now state whether offset_bytes counts from the start of the full output log or from the start of the retained in-memory buffer (which keeps only the tail of the output), so offset semantics are unambiguous.
