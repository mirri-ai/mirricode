---
"@mirri-ai/mirri-code": patch
---

Fix tool call validation failure when AI models serialize numeric arguments as strings (e.g., "line_offset": "178" instead of 178). Add schema-based argument conversion to improve LLM response tolerance.
