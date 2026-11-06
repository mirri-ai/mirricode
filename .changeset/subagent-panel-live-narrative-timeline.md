---
"@mirri-ai/release": patch
---

web: Fix the sub agent panel showing all tool calls collapsed into one block out of execution order, and the narrative area repeatedly re-rendering while a sub agent was running. The panel now shows the member's reasoning, text and tool calls interleaved in the true order they happened, and streaming output stays visually stable while tools run.
