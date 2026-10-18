---
"@mirri-ai/release": major
---

Remove the managed account login. The `mirri login` and `mirri auth` subcommands and the `/login` and `/logout` slash commands are gone, the web UI opens a provider setup page instead of a sign-in gate, and ACP clients no longer see account authentication. Providers previously set up with OAuth need an explicit API key.
