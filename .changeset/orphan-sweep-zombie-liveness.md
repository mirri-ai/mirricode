---
"@mirri-ai/release": patch
---

Fix the Linux startup orphan sweep treating an already-terminated process as still alive. A killed child that has not been reaped yet keeps its `/proc` entry and process-group id, so the sweep re-verified it as a live orphan, left its task archive `Running`, and burned the full grace window before escalating to `SIGKILL`. Zombie processes now probe as dead and no longer count as live process-group members, so such archives settle `Stopped` (and are disposed under the terminate policy) immediately.
