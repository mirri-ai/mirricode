---
"@mirri-ai/mirri-code": patch
---

Fix MCP server enable/disable in settings failing with "Session was not found". The toggle was routed through session-scoped RPCs that picked an arbitrary session (often stale/destroyed), causing the error. Now persists the `enabled` flag directly to `mcp.json` via new global RPCs (`toggleGlobalMcpServer`) that trigger a global reload — no session dependency, affects all sessions uniformly, and survives restarts.
