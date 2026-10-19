---
"@mirri-ai/release": patch
---

Fix the desktop app's native integrations (open in app, reveal in file manager, notifications) failing with "no native launch channel" after the app window loaded, restoring the desktop bridge that the build had accidentally broken.
