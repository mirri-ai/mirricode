---
"@mirri-ai/release": minor
---

Make the essential default `agent` profile non-dispatchable: spawning `agent` as a subagent is now refused at the dispatch gate with a dedicated message, the profile no longer appears in dispatch allowlists or the subagent profile listing, and the built-in allowlist seed no longer includes it. A same-name user override re-adding `agent` to its own allowlist cannot lift the refusal — users who want a fully-capable dispatchable agent should define their own profile (any name other than `agent`). User-defined profiles with equivalent capabilities remain dispatchable as declared.
