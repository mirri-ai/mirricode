---
"@mirri-ai/release": patch
---

web: Fix the scheduled task edit form rolling back to pre-save values after a save, a saved schedule reopening as the generic "custom" option instead of the option the user picked (an every-minute rule now reads "every minute" on every surface), and confirming a custom schedule rule silently doing nothing.
