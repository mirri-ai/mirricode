---
"@mirri-ai/release": patch
---

Fix the ACP conformance overlay tests hanging on exit: disposing an engine (the `mirri acp` / `mirri serve` shutdown paths and the NAPI dispose) now tears down session-scoped MCP overlay connections and reaps their child processes instead of letting them outlive the host connection.
