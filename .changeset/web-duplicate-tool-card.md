---
"@mirri-ai/release": patch
---

web: Fix a duplicated tool group rendering when the same subagent dispatch step reached the transcript twice (e.g. after a service crash and recovery) — each tool call now renders once, with the live progress kept on the surviving card.
