---
"@mirri-ai/mirri-code": patch
---

Add GET /api/v1/tools/all endpoint returning builtin tool descriptors without requiring an active session. The web Settings panel uses this instead of GET /tools so session-scoped tool listing is no longer overloaded for profile management.
