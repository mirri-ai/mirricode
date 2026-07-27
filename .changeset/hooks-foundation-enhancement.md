---
"@mirri-ai/mirri-code": patch
---

Add `failClosed` config option to hooks for blocking on failure instead of allowing. Add `additionalContext` output field parsed from hook results. Enrich hook payloads with `last_assistant_message` (Stop), `duration_ms` (SubagentStop), and `context_window_size` (PreCompact).
