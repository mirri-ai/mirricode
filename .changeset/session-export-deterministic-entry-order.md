---
"@mirri-ai/release": patch
---

Make the session-export archive deterministic: entries are now ordered by name instead of following the session directory's filesystem iteration order, so two sessions with identical content export an identical entry list and zip layout.
