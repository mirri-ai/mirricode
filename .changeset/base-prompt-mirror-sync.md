---
"@mirri-ai/release": patch
---

core: Sync the TS base prompt template with the rust mirror (8 stale sections refreshed) and add cross-engine drift guards in both engines' test suites — the two files feed the same settings surface, so future one-sided edits now fail tests instead of silently diverging.
