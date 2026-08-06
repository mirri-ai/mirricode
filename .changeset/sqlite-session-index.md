---
"@mirri-ai/mirri-code": patch
---

Speed up v2 session list loading by serving summaries from a SQLite index instead of reading every session's file on each request.

Speed up v2 startup by reconciling the SQLite index incrementally — unchanged sessions are skipped via state-file mtime, and the disk sweep runs synchronously, cutting a warm no-change startup reconcile from ~13s to a few milliseconds.