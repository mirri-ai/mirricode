---
"@mirri-ai/release": patch
---

Fix sessions that stayed in the sidebar after their files were deleted outside the app — opening such a session now also removes it from the list, instead of failing on every click and coming back on the next reload. Sessions that were created but never used, archived sessions with a compressed history, and sessions still open in the app are left untouched.
