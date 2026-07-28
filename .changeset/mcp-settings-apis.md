---
"@mirri-ai/mirri-code": minor
---

Fix MCP server status showing "error" in the desktop app: the daemon now resolves the login shell PATH before spawning stdio MCP processes, so commands like `npx`, `uvx`, and `node` are found.

Redesign the MCP settings panel: structured env-var editor with add/modify/delete per key, `${VAR}` pattern highlighting, per-server test-connect button, per-server connect button with polling status updates, and removal of comma-separated tool text inputs in favor of per-tool toggles.

Add MCP server management APIs: read raw (unexpanded) config, get global toggle state, enable/disable individual tools, test-connect to an MCP server, and connect a single server by name. The settings list API returns immediately without waiting for connections.

Fix surrogate pair splitting in compaction truncation: `truncateTextToTokensFromEnd` now uses `charCodeAt` instead of `codePointAt` for surrogate detection, preventing emoji and other supplementary-plane characters from being split mid-pair.
