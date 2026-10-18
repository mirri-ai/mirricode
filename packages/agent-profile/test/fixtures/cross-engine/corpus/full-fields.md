---
name: full-fields
description: A profile exercising every supported frontmatter field.
whenToUse: Use for full-surface testing of the agent profile parser.
defaultModel: claude-sonnet
tools:
  - Read
  - Grep
  - Glob
disallowedTools:
  - Write
  - Edit
subagents:
  - coder
  - explore
capabilitiesRequired:
  - code.read
  - code.explore
enabled: false
---

You are a full-surface test agent.

Second paragraph of the prompt.
