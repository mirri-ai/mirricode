---
name: roundtrip-hostile
description: 'Hostile scalars: colon-space, #hash, and, many, commas'
whenToUse: "  padded with leading and trailing spaces  "
defaultModel: 'claude-sonnet'
tools:
  - 'tool:with:colons'
  - '#leading-hash'
  - ' padded item '
  - 'true'
  - 'null'
  - '42'
---

You survive hostile scalars: a # mid-value, https://example.com/path, and a trailing detail:
