---
"@mirri-ai/release": patch
---

Fix plan-mode approval so exiting plan mode waits for the user's decision instead of auto-exiting. Approve, revise, or reject-and-exit now work as intended on the web plan-review card, and an approval request in an interactive turn now shows on the approval surface even when the reverse-RPC channel is briefly unavailable.
