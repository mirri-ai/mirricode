---
'@mirri-ai/release': patch
---

Fix subagents spawned mid-session starting without the session's connected MCP tools, which left a subagent profile's MCP tool authorization unusable until the app restarted. A newly spawned subagent now receives the tools the session already has connected.
