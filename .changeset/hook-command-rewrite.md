---
"@mirri-ai/mirri-code": minor
"@mirri-ai/agent-core": minor
---

Add experimental hook command rewriting: PreToolUse shell hooks can now return `updatedInput` to modify tool arguments before execution (e.g. rtk integration). Enable with `MIRRICODE_EXPERIMENTAL_HOOK_COMMAND_REWRITE=true`.
