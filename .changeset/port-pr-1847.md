---
"@mirri-ai/mirri-code": patch
---

Fix LaTeX formulas rendering as garbled overlapping text when the web UI is accessed over the network; the server's content security policy now allows the inline styles that math and code highlighting rely on, while scripts remain strictly restricted.

Ported from MoonshotAI/kimi-code#1847.
