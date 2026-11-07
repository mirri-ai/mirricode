---
"@mirri-ai/release": patch
---

Fix goal usage being tracked after the goal is completed: post-completion turns no longer record goal usage entries in the session log, and a completed goal can no longer flip to blocked when an old budget limit is reached.
