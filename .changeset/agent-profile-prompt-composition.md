---
"@mirri-ai/release": patch
---

Fix saving a built-in agent profile from the settings page replacing the agent's full system prompt with a generic placeholder sentence — a saved profile now keeps the shared system prompt and layers its own role text on top, and saving with an empty prompt preserves the default behavior instead of degrading it. Existing profile files already carrying the placeholder are repaired automatically on load.
