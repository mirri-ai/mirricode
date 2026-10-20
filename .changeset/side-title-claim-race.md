---
"@mirri-ai/release": patch
---

Fix session titles sometimes staying untitled: when the first-turn title generation was still running as the turn ended, its retry was skipped, so a failed generation never fell back to naming the session from your prompt.
