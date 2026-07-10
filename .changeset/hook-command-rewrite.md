---
"@mirri-ai/mirri-code": minor
"@mirri-ai/agent-core": minor
---

Add experimental hook command rewriting via `RewriteToolInput` event: shell hooks can now modify tool arguments before execution (e.g. rtk integration). Enable with `MIRRICODE_EXPERIMENTAL_HOOK_COMMAND_REWRITE=true`.
