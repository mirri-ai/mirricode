---
"@mirri-ai/release": patch
---

Require the agent to load a matched skill's full instructions before acting on it, instead of acting on the skill's one-line description. After two failures of the same operation, the agent now stops retrying with varied arguments and re-reads the skill for a missed mechanism (context selection, environment switching, multi-instance routing).
