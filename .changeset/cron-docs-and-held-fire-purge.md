---
"@mirri-ai/release": patch
---

Fix a deleted cron task's held fire still being injected after the running turn ends: fires held while a session is busy are now purged when their task is deleted mid-hold. Also align the CronCreate/CronList tool descriptions and the scheduled-tasks documentation (en/zh) with the actual behavior: recurring defaults to false, the JSON `{"tasks":[...]}` CronList shape with `stale`, `cron-<uuid>` ids, and no phantom limits or fields inherited from the retired TypeScript engine.
