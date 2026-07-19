---
"@mirri-ai/mirri-code": patch
---

Fix AGENTS.md files installed as symbolic links being ignored by the profile context loader.

The `isFile()` helper in the profile context called `kaos.stat(path)` without
explicitly opting into symlink resolution. When AGENTS.md files are installed
as symbolic links (common with dotfile managers such as `stow`), they could be
treated as non-regular files and skipped. The fix passes
`{ followSymlinks: true }` to `kaos.stat()` so symlinked AGENTS.md files are
resolved to their targets and loaded as expected.

Ported from MoonshotAI/kimi-code#1840
