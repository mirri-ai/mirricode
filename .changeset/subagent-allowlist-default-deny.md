---
"@mirri-ai/release": minor
---

Change the agent profile `subagents` field to default-deny semantics: a profile without an explicit `subagents` allowlist can no longer dispatch any subagent (previously it could dispatch all of them). The built-in main agent ships with the built-in subagents allowlisted, so the out-of-the-box experience is unchanged; users who relied on the old "omitted = all" behavior must add the subagent names they want to allowlist.
