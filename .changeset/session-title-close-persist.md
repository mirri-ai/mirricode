---
"@mirri-ai/release": patch
---

Fix a session title set just before the session was closed being lost on resume. The restored session looked untitled and ran the title model call again on its next turn; the close path now persists the current metadata before tearing the session down.
