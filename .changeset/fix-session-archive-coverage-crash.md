---
"@mirri-ai/release": patch
---

Fix a daemon crash when archiving a session whose wire log contains a step that began but never ended: replaying such a session left the token-coverage counter pointing past the end of the reconstructed message history, and the next context-token read panicked and killed the server (web showed "not connected" and realtime reconnect failures). The coverage counter is now retracted whenever history is shortened, and the pending-token read clamps defensively so a future invariant break degrades to a mis-estimate instead of a crash.
