---
"@mirri-ai/release": patch
---

Fix forked sessions disappearing from the session list after a web refresh: forking a conversation now records the fork in the session index immediately, so the forked session stays visible instead of being treated as an empty session until its first message.
