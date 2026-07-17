---
"@mirri-ai/mirri-code": patch
"@mirri-ai/mirri-code-sdk": patch
---

web: Replace the five-value session status enum with orthogonal work facts (busy, main_turn_active, pending_interaction, last_turn_reason) to prevent race conditions where status transitions were lost or duplicated during streaming.
