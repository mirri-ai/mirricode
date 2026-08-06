---
"@mirri-ai/agent-core-v2": patch
---

Tolerate unknown hook event names in the config `[[hooks]]` section instead of silently dropping every registered hook. Events not in the v2 engine's supported list (`PreLlmRequest`, `PostLlmRequest`, `RewriteToolInput`) are now filtered out individually with a warning logged, while all other valid hooks continue to work.