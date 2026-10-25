---
"@mirri-ai/release": patch
---

Fix the server auth token file being left empty or half-written after an unclean shutdown. The token is now written durably before the server reports it ready, and two servers starting at the same time no longer overwrite each other's pending write.
