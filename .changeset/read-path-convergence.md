---
"@mirri-ai/mirri-code": patch
---

Speed up v2 session listing, especially on cold starts: concurrent first-screen requests used to re-read the workspace catalog, session index, and session directories from disk on every call; the workspace registry and alias resolution are now cached per request window, the session index snapshots its directory walk after startup reconcile, and the SQLite store runs in WAL mode with a startup checkpoint so a leftover journal never stalls the first reads.