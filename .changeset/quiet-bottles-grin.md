---
"@mirri-ai/mirri-code": minor
---

Expand `${VAR}` / `${env:VAR}` environment-variable references in MCP server config values, matching the v1 engine behavior. Use `${VAR}` or `${env:VAR}` in mcp.json strings (command, args, env, url, headers, …) to inject environment values; the effective config now resolves them at load time.
