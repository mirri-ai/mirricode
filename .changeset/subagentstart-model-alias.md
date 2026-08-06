---
"@mirri-ai/agent-core-v2": minor
---

Report the bound model in the `SubagentStart` external hook payload. `[[hooks]]` scripts for `SubagentStart` can now read `model_alias` from the payload to see which model the subagent run was launched with.