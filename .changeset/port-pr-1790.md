---
"@mirri-ai/mirri-code": patch
---

Fix repeated request rejections after an interrupted model response by recording tool calls that never ran and closing them with an interrupted result.

Ported from MoonshotAI/kimi-code#1790.
