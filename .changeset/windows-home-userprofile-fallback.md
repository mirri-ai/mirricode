---
"@mirri-ai/release": patch
---

Fix the engine refusing to start on Windows when the `HOME` environment variable is not set (the default outside Git Bash) — the data directory now falls back to `USERPROFILE`.
