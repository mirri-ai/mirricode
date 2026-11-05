---
"@mirri-ai/release": patch
---

Fix known RustSec advisories in the engine's bundled dependencies: upgrade the zip archive handling to a patched release (session export and plugin install archives) and refresh the zstd decompression stack. Also add a `cargo-deny` CI gate so future dependency advisories, license issues, or non-crates.io sources fail the build before release.
