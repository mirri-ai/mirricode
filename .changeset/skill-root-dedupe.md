---
"@mirri-ai/release": patch
---

Fix hundreds of "skill shadowed by a later discovery" warnings spamming the log on every startup when the workspace root is the user's home directory — the same skill folders were scanned once as project-level and again as user-level sources. Skills load exactly as before; the duplicate scans and their warnings are gone.
