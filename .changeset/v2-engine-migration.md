---
"@mirri-ai/mirri-code-sdk": minor
"@mirri-ai/mirri-code": minor
"@mirri-ai/kap-server": minor
"@mirri-ai/klient": minor
"@mirri-ai/acp-server": minor
"@mirri-ai/agent-core-v2": minor
---

v1→v2 engine migration: the SDK and CLI now run the agent-core-v2 engine by default.

- `mirri-code-sdk` gains a v2 backend (`createMirriHarnessV2`) over the same
  `MirriHarness` surface: session lifecycle, turn driving, plan/goal/cron/
  background, skills, plugins, MCP, approvals and questions are all bridged
  from the v2 engine to the frozen v1 wire contract.
- The CLI's TUI defaults to the v2 engine. Set `MIRRICODE_LEGACY_FLAG=1` to
  force the legacy agent-core backend during the migration window.
- `kap-server` is now the only server (`mirri web`); the v1 `packages/server`
  and its `mirri server` command tree were retired (`mirri server` prints a
  deprecation notice and exits; `mirri web` is the replacement).
- New `@mirri-ai/acp-server` package: a v2-native Agent Client Protocol host
  backed directly by agent-core-v2.
- `@mirri-ai/klient` facade extended with skills, thinking, and the matching
  wire-contract entries.