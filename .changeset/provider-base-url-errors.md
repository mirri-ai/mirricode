---
"@mirri-ai/release": patch
---

Fix imported third-party providers silently sending requests to the official OpenAI endpoint when no base URL was configured: the engine now fails fast at provider creation with a clear "no base URL configured" error instead of retrying into a network failure. First-party providers (openai, anthropic, google-genai, kimi) keep their official default endpoints, and the models.dev import persists the catalog's known endpoint automatically.
