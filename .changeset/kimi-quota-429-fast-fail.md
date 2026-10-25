---
"@mirri-ai/release": patch
---

Kimi quota-exhausted 429s now fail fast: video uploads and chat requests surface a terminal `provider.quota_exhausted` error (with the provider's Retry-After hint) instead of being retried as generic rate limits.