---
"@mirri-ai/agent-core-v2": patch
"@mirri-ai/kap-server": patch
"@mirri-ai/mirri-code": patch
---

Fix archiving a session making the sidebar's Load more count overshoot: workspace session totals now count only active (non-archived) sessions, and pagination recovers when the paged cursor session was archived.
