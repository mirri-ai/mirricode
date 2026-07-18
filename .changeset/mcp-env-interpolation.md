---
"@mirri-ai/mirri-code": patch
---

Support `${VAR}` and `${env:VAR}` environment variable interpolation in string values of `mcp.json`, so secrets and hostnames no longer need to be hardcoded. Undefined variables resolve to empty strings and invalid results are rejected by schema validation.
