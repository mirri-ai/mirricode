---
"@mirri-ai/mirri-code": patch
---

Fix custom agent profile deletion failing silently when the profile file is missing or already removed. The delete route now returns a proper error envelope instead of a raw 500 for unrecognized errors.
