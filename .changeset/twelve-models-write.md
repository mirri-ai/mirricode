---
"@mirri-ai/release": minor
---

Give chatty reasoning models a per-model prompt that you can read and edit. A built-in rule now materializes one Markdown file per model under `~/.mirri-code/model_prompts/`, covering models such as `mimo-v2.6-pro` and `mimo-v2.6-flash` so they go straight to action instead of narrating their reasoning. The file is appended to the system prompt of every new conversation that uses that model, is shared by all providers reaching the same model, and is never overwritten once it exists — your edits survive product updates. Clear a file to give that model no extra prompt. Set `disable_model_prompts = true` in `config.toml` to stop injecting any of them.

web: The model editor now shows and edits a model's prompt, and states that edits apply to all providers using that model and take effect in new conversations.
