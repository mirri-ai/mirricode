---
"@mirri-ai/release": patch
---

Fix session recording failing on Windows with "access denied" (os error 5) — the session's `wire.jsonl` log was created but every write was rejected. Messages sent in a session are now persisted again on Windows.
