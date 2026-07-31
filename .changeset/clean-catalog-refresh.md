---
"@mirri-ai/mirri-code": patch
---

Rework catalog refreshes to only append new models; never modify or delete existing aliases. Remove the `overrides` sub-object on model aliases (legacy configs are migrated into top-level fields on load). Deleted models are remembered per-provider so a refresh cannot silently re-add them.
