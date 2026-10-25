---
"@mirri-ai/release": patch
---

web: Fix contradictory status shown for parallel subagents — a finished member kept the pulsing "working" indicator forever, while a still-running member showed the completed checkmark with a "Working" summary. Member status now comes from a single source, and rows whose finish event was missed are reconciled closed on the next refresh instead of staying stuck.
