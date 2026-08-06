---
"@mirri-ai/mirri-code": minor
---

Custom agent profiles are now defined as Markdown files with YAML frontmatter — the Markdown body becomes the system prompt. Legacy `.yaml`/`.yml` profiles are still read for backward compatibility, and a `.md` file takes priority when both exist for the same agent name.
