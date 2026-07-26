---
"@mirri-ai/mirri-code": patch
---

web: Fix blank screen caused by the macOS titlebar wrapper breaking the app grid layout. The desktop titlebar now uses a grid row instead of a flex wrapper, restoring the sidebar and conversation panes on all platforms.
