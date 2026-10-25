---
"@mirri-ai/release": patch
---

Update TLS certificate handling for a security fix: bump `webpki-roots` to 1.0.10, which removes the compromised `emSign ECC Root CA - G3` root (October 2026 eMudhra intermediate CA key compromise), and upgrade `reqwest` to 0.13 so HTTPS connections now validate against the operating system's trust store instead of a bundled root set.
