---
"@mirri-ai/mirri-code": patch
"@mirri-ai/mirri-web": patch
---

Optimize the unit formatting of the context usage display: switch from 1000-based to 1024-based units (262144 → "256k"), use ceiled whole-number percentages, and share a single formatter across TUI and web.
