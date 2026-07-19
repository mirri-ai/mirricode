---
"@mirri-ai/mirri-code": patch
---

Fix Esc and Ctrl+C cancelling an in-progress compaction instead of closing an open /btw panel first. The /btw panel stacks above the transcript, so it is now dismissed before any compaction or stream cancel logic runs.

Ported from MoonshotAI/kimi-code#1811.
