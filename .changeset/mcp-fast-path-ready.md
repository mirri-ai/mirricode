---
"@mirri-ai/agent-core-v2": patch
"@mirri-ai/mirri-code": patch
---

Defer MCP readiness out of session and agent creation so a new workspace's first message is no longer blocked while MCP servers connect; readiness is awaited at the first LLM step instead.