---
"@mirri-ai/release": minor
---

Add multi-select delete to the archived-sessions management page: check several sessions (select-all over the visible, filtered set included) and delete them in one confirmed action. Selected ids that leave the list before the confirm — deleted or restored elsewhere, or moved on by auto-compression — are excluded and named in the result instead of erroring. A failed restore no longer refreshes the list out from under the user, and the compressed view's column alignment was fixed.
