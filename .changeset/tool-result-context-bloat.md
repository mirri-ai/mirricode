---
"@mirri-ai/release": patch
---

Fix oversized tool outputs silently filling the context window: results over 50K characters are now saved to disk and replaced with a short preview the agent can page through with `Read`, and single-line `Grep` matches (minified JSON, bundled CSS) are capped instead of blowing past the output budget.
