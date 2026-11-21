---
"@mirri-ai/release": patch
---

Fix the engine crashing when deleting a workspace while its skill directories were still settling file events. The crash required restarting the engine manually; deleting a workspace now always shuts the watcher down cleanly.
