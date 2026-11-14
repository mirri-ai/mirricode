---
"@mirri-ai/release": patch
---

web: Fix the thinking-effort picker being replaced by a plain "always thinks" note for models that cannot turn thinking off but do declare effort levels, such as GLM 5.3. The effort levels (low/high/max) are selectable again, with the "cannot be turned off" explanation shown as a tooltip on the picker.
