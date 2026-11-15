---
"@mirri-ai/release": patch
---

web: Fix a resumed background subagent disappearing from the bottom task dock. A subagent dispatched in the background now stays a background subagent when it is resumed, and the dock keeps showing it — with its live status — instead of freezing it at the finished run or listing it twice.
