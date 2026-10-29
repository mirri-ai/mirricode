---
"@mirri-ai/release": minor
---

On Windows, the Bash tool now runs commands through Git Bash instead of cmd (matching the documented behavior), and errors with actionable guidance when Git Bash is missing. A new PowerShell tool (Windows only) runs native PowerShell commands with faithful exit codes and UTF-8 output. Grep now falls back to the cached ripgrep binary on Windows.
