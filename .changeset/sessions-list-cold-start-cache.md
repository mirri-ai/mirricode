---
"@mirri-ai/mirri-code": minor
---

Speed up v2 session listing on cold starts: the workspace catalog and alias resolution are now cached for the process lifetime and warmed before the server listens, so the first session list no longer blocks on disk reads; manual refresh re-syncs external writes and expands legacy alias buckets.