---
"@mirri-ai/release": patch
---

Fix terminal panel failing with "session not found" for sessions outside the default workspace (the web/desktop terminal panel now resolves sessions through workspace routing, including on-demand resume after a server restart).
