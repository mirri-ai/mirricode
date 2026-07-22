---
"@mirri-ai/mirri-code": minor
---

Add LLM-aware model selection for subagent dispatch: models with a `description` field in `config.toml` are now exposed in the `Agent` and `AgentSwarm` tool descriptions, so the LLM can make informed decisions when overriding the default model. Agent profiles display their `defaultModel` in the tool description, and invalid model aliases return an actionable error listing valid options.
