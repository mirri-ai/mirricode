---
"@mirri-ai/release": patch
---

web: Make tool-call rows lead with the command itself. A Bash call that starts by changing into the current workspace now shows that directory as a badge instead of repeating its full path, and other directories collapse to `…/folder/`, so consecutive tool calls are distinguishable at a glance. The hover view of a call is now as wide as the conversation column, capped at four lines with the rest scrollable.
