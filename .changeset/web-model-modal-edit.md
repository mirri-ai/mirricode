---
"@mirri-ai/mirri-code": patch
---

Web: convert model add/edit from inline forms to modal dialogs above the fullscreen settings overlay. Fix auto-save not triggering when clearing the LLM exposure toggle (description field) — cleared override fields now send an empty string instead of `undefined` so the server merge actually applies the deletion. Fix model edit form not reflecting saved overrides by resolving `overrides` into top-level fields when populating the form, matching the runtime's `effectiveModelAlias` merge semantics.
