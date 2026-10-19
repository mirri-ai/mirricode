---
"@mirri-ai/release": patch
---

Fix live goal and skill-activation updates not appearing in a forked session until the page was refreshed. The forked session's agent was announcing these changes on the original session's event stream, so its own clients never received them in real time.
