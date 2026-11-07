---
"@mirri-ai/release": patch
---

Make the agent's default reasoning discipline sharper, so reasoning models spend less time on deliberation that does not change the answer. Reasoning now stops once the next action and its arguments can be named; a chosen approach is carried through instead of re-litigated; recognized hedging loops ("wait", "let me double-check") are dropped; and a finished edit is no longer re-read to confirm it landed — verification means running the check that covers the change.
