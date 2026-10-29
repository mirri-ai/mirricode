---
"@mirri-ai/release": minor
---

Add interrupted-task recovery engine: lifecycle sentinel detects graceful vs abnormal service exit, identifies sessions whose turns were interrupted at exit, backfills an `interrupted` turn outcome, and exposes `recovery.*` RPCs to list, resume, and dismiss interrupted sessions.
