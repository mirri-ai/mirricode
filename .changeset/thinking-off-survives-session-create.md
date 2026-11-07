---
"@mirri-ai/release": patch
---

Turning thinking off in the new-session composer now survives the first message. The draft level was never sent when the session was created, so the engine applied the model's declared default and thinking came back on — most visibly on reasoning models such as MiMo.

The pick also survives a restart now. It was never written to the session's journal, so the agent rebuilt on resume had no record of it and reverted to the model's default. Both the create-time record and any later pick from the thinking control are now persisted and re-applied.

The session's recorded level also folds back into the composer correctly. The engine reported `"none"` for both "the user turned thinking off" and "no level was ever set", leaving clients unable to tell the two apart. `session.status` answers `null` for the unset case, so an explicit off is preserved instead of being read as a missing preference.
