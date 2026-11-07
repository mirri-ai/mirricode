---
"@mirri-ai/release": patch
---

Turning thinking off in the new-session composer now survives the first message. The draft level was never sent when the session was created, so the engine applied the model's declared default and thinking came back on — most visibly on reasoning models such as MiMo.

The session's recorded level also folds back into the composer correctly now. The engine reported `"none"` both for "the user turned thinking off" and for "no level was ever set", leaving clients unable to tell the two apart. `session.status` answers `null` for the unset case, so an explicit off is preserved instead of being read as a missing preference.
