---
"@mirri-ai/release": patch
---

Fix a first-launch race after an upgrade where importing legacy scheduled tasks could overlap the first task-list reads, briefly showing an incomplete set of imported tasks.
