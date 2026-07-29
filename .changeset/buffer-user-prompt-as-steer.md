---
"@mirri-ai/mirri-code": patch
---

Fix "turn.agent_busy" error when sending a follow-up message while the model is still thinking — user prompts are now automatically buffered as steers instead of being rejected.
