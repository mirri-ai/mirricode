---
"@mirri-ai/mirri-web": patch
"@mirri-ai/kap-server": patch
"@mirri-ai/protocol": patch
---

Port from kimi-code #1609: fix(web): make mid-turn delta offsets step-relative

Fixes mid-turn delta offsets to be step-relative, improving snapshot resync behavior during multi-step turns.
