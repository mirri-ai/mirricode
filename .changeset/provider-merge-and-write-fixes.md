---
"@mirri-ai/mirri-code": patch
---

Prevent catalog re-imports from resurrecting user-deleted models or overwriting hand-written model records, and serialize provider writes so concurrent operations cannot lose each other's changes.
