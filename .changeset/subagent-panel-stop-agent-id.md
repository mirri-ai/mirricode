---
"@mirri-ai/release": patch
---

Fix the subagent panel's stop button failing with "task not found" — it now stops foreground swarm members and members whose panel was rebuilt after a page reload, instead of reporting an error.
