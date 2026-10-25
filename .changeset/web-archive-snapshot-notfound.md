---
"@mirri-ai/release": patch
---

Fix the web UI showing a scary "Cannot load current session" warning when archiving a session: the in-flight snapshot sync error from the engine (JSON-RPC code -32001) is now recognized as session-not-found and handled gracefully — the session is silently removed from the list and the app switches to the next session without any warning toast.
