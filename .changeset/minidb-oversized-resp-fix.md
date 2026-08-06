---
"@mirri-ai/mirri-code": patch
"@mirri-ai/minidb": patch
---

Make the embedded key-value server handle large RESP requests without quadratic re-buffering: oversized requests are rejected as soon as their declared length is read, and allowed large payloads stream in with linear copy cost.
