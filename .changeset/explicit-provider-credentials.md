---
"@mirri-ai/release": minor
---

Require explicitly configured provider credentials: the engine no longer reads `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GOOGLE_API_KEY` or `MIRRI_API_KEY` from the shell environment implicitly. A provider whose credential is neither in the credential store nor in its `env` sub-table in `config.toml` now fails with an actionable "please configure credentials" error pointing to Settings → Providers. Users who relied on shell environment variables should add them once through the provider settings (or as `env` sub-table entries).
