---
"@mirri-ai/mirri-code": minor
---

Add a tool capability abstraction so external MCP tools can be preferred over built-ins via a layered `integrations.yaml` (user-global `~/.mirri-code/integrations.yaml` + project-local `<project>/.mirri-code/integrations.yaml`, where project overrides user). Declare MCP tool capabilities (e.g. `code.explore`) to have them surfaced ahead of the built-in Grep/Glob in the system prompt. Provider ordering is now driven purely by `preferOver`; the earlier hardcoded MCP-first tiebreaker has been removed (existing configs that already declare `preferOver` are unaffected).
