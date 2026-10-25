---
"@mirri-ai/release": patch
---

Fix two stale statements in the agent's built-in guidance: secret files are escalated through the permission flow rather than refused outright by the file tools, and context compaction keeps a recent retention tail plus the user's messages rather than dropping a middle block.
