---
"@mirri-ai/release": patch
---

Fix TaskOutput and TaskStop failing with "task not found" when the agent sent a background task id without its `task-` prefix. The engine now recognizes the bare id and reads or stops the task as intended.
