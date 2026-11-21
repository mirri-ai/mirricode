---
"@mirri-ai/release": patch
---

Update the timezone database used for scheduled tasks to the current IANA release (the previous embedded database stopped receiving updates). Schedules keep firing at the same local times; DST edge cases behave as before.
