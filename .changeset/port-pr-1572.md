---
"@mirri-ai/mirri-code": patch
---

web: Fix sessions getting stuck in a sending state after a reconnect, so the working spinner stops and the next message sends normally once a turn finishes while the connection is down.

Ported from MoonshotAI/kimi-code#1572
