---
"@mirri-ai/mirri-code": minor
---

web: Load sessions progressively — the most recently used workspace renders first, the rest fill in behind it — and add a per-workspace refresh button to re-sync folders on demand.

Serve session reads from the local SQLite session index so opening the web UI no longer rescans the session folders.