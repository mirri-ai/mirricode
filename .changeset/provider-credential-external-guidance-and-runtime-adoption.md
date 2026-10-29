---
"@mirri-ai/release": minor
---

Provider credentials are safer and clearer in three ways.

When a provider's credential comes from an external source like the Azure CLI and that source fails, the error now tells you what to do (for example, run `az login`) instead of silently falling back to a possibly wrong API key from the environment.

Saving a provider with an API key through `provider.set` or a catalog import no longer writes the key into config.toml: the key moves straight into the credential store, and the config file stays free of plaintext secrets.

Small fixes: the `credential list` CREATED column now shows when each credential was created rather than last updated, and a defensive downcast on LLM factories returns "not found" instead of crashing on test doubles.
