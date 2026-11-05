---
"@mirri-ai/release": patch
---

Fix a startup crash when two mirri processes (for example the desktop app and the CLI) start at the same time right after an upgrade: one of them could refuse to start with a "duplicate column name" error instead of waiting for the other to finish preparing the session library.
