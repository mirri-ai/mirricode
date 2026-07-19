---
"@mirri-ai/mirri-code": patch
---

Port from kimi-code #1791: fix(security): close FetchURL SSRF bypasses and DNS-rebinding window. Harden the built-in URL fetch tool so crafted domains and redirect chains can no longer reach loopback or internal network services: hostnames are resolved and every address is checked against loopback / RFC1918 / link-local / CGNAT / ULA ranges (including IPv4-mapped IPv6 forms), redirects are followed manually with the safety check re-run on every hop, and each request's connection is pinned to the validated DNS answers so a connect-time re-resolution cannot be rebound to an internal address.
