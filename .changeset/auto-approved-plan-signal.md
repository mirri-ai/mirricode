---
"@mirri-ai/agent-core": patch
"@mirri-ai/mirri-code": patch
---

In auto permission mode, plan exits are now marked as auto-approved (not user-reviewed) in both the tool result and the transcript, so the agent no longer treats automatic plan approval as a user signal to start executing.
