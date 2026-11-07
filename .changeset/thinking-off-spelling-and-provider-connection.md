---
"@mirri-ai/release": patch
---

Turning thinking off now actually reaches the provider. Previously the off request omitted the reasoning field entirely, which every OpenAI-compatible vendor reads as "use the default" — reasoning stayed on with no visible signal. Each endpoint's own spelling is now sent (`thinking.type: disabled`, `reasoning_effort: none`, or `enable_thinking: false`), inferred per vendor with a per-model override (`thinking_off`) for gateways.

Models that cannot be turned off no longer offer an off switch: the composer shows only their effort tiers, with a note saying the model always thinks.

Provider connection settings — protocol type, endpoint, custom headers, thinking-off — are now editable in one dialog instead of requiring the provider to be deleted and recreated, which also keeps the configured model list. Switching the protocol no longer loses models or credentials.

The global `[thinking]` config section is retired: its `effort` duplicated each model's `default_effort`, `enabled` duplicated per-model closeability, and `keep` was never implemented. A leftover section is preserved in your file but has no effect, and `mirri doctor` says so. `MIRRICODE_MODEL_THINKING_EFFORT` and `MIRRICODE_MODEL_THINKING_KEEP` are removed with it.

New sessions now inherit the model's declared default effort instead of starting with none.
