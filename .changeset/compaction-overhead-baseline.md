---
"@mirri-ai/release": patch
---

Fix auto-compaction under-counting context after a compaction or undo: the fixed per-request overhead (system prompt and tool table) is now kept in the pressure measure, so the context no longer reports as mostly-empty right after compacting when the next request is actually near the window limit.
