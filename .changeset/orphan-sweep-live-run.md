---
"@mirri-ai/release": patch
---

Fix a manually triggered task run being reported as failed with the restart cause while it was still running. When a run was started right after the app launched — before the scheduler's first pass — that pass treated the live run as left over from a previous process and rewrote it to failed, so the run history showed "服务重启中断" for a run that went on to complete normally.
