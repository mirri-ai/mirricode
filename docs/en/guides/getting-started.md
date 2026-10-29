# Getting started

## What is Mirri Code

Mirri Code is an AI agent that helps you carry out software development tasks — reading and modifying code, running shell commands, searching files, fetching web pages, and autonomously planning and adjusting its next steps based on feedback as it works. It ships primarily as a desktop app, with a `mirri` CLI for the terminal.

It fits scenarios such as:

- **Writing and modifying code**: implementing new features, fixing bugs, completing refactors
- **Understanding a project**: exploring an unfamiliar codebase and answering questions about architecture and implementation
- **Automating tasks**: batch-processing files, running builds and tests, chaining multiple scripts together

The CLI is written in Rust. The official install script ships a self-contained native executable that requires no pre-installed runtime; the npm channel is retired (see the migration note below).

## Installation

Mirri Code's primary distribution is the desktop app; the CLI is also available via the official install script. The npm channel is retired and remains available to existing users only.

### Desktop app (recommended)

Download the installer for your platform (macOS `.dmg` or `.zip`, Windows `.exe`, Linux `.AppImage` / `.deb`) from [GitHub Releases](https://github.com/mirri-ai/mirricode/releases/latest) and install it. The macOS package is signed and notarized.

### Install script

Install the CLI from a terminal — no Node.js required:

- **macOS / Linux**:

```sh
curl -fsSL https://mirricode.com/install.sh | bash
```

- **Windows (PowerShell)**:

```powershell
irm https://mirricode.com/install.ps1 | iex
```

> On Windows, install [Git for Windows](https://gitforwindows.org/) before first launch for the Bash tool (Unix syntax); the PowerShell tool is available out of the box. If Git Bash is in a custom location, set `MIRRICODE_SHELL_PATH` to the absolute path of `bash.exe`.

The script automatically downloads the latest release, verifies the checksum, and places the `mirri` executable on your `PATH`.

::: warning Note
The script locates GitHub Releases by `v`-prefixed tag. Until the first `vX.Y.Z` Release is published, resolving the latest version 404s — this is expected, use the desktop installer in the meantime. Pinning an old version (e.g. `1.10.0`) also 404s, because old releases use scoped tags and likely have no `manifest.json`.
:::

### Migrating from npm

The npm packages (`@mirri-ai/mirri-code`, `@mirri-ai/mirri-node`) are retired after their last release:

- **Existing installs keep working** — npm installations and runs are unaffected; no new versions are published, and no update prompts will appear.
- **Move to the desktop app** — download the installer from [GitHub Releases](https://github.com/mirri-ai/mirricode/releases/latest), or use the [install script](#install-script) for the CLI. Configuration is stored in `~/.mirri-code/` (see [Where data is stored](#where-data-is-stored)) and carries over as-is.
- To uninstall the npm package: `npm uninstall -g @mirri-ai/mirri-code`.

## Upgrade and uninstall

After installation, verify that the executable is ready:

```sh
mirri --version
```

**Upgrade**: re-run the install script to overwrite with the latest release (the CLI has no `mirri upgrade` subcommand). Desktop users download the new installer from [GitHub Releases](https://github.com/mirri-ai/mirricode/releases/latest) and install over the old version.

**Uninstall**: if you installed via the script, delete the `mirri` executable (default: `~/.mirri-code/bin/mirri`). If you installed via npm:

```sh
npm uninstall -g @mirri-ai/mirri-code
```

## First launch

Open the desktop app to get started. If you prefer the browser, start the local web UI from your project directory:

```sh
cd your-project
mirri web
```

`mirri web` runs the engine in the foreground and opens the web UI in your browser; press `Ctrl-C` to stop it. To run a single instruction without opening any UI, use `mirri print`:

```sh
mirri print "Take a look at this project's directory structure"
```

On first launch you need to configure a model provider. In settings, add a provider and its API key (for example Mirri AI Open Platform) or a custom registry, then select a default model. Providers configured in `~/.mirri-code/config.toml` work the same way.

::: tip Using other AI providers
If you want to connect Anthropic, OpenAI, Google, or other providers, edit `~/.mirri-code/config.toml` directly to configure the API key. See [Providers and models](../configuration/providers.md) for details. For the full reference of all config options, see [Configuration files](../configuration/config-files.md), [Environment variables](../configuration/env-vars.md), and [Configuration overrides](../configuration/overrides.md).
:::

## Your first conversation

Once a model is configured, describe a task in natural language. A good starting point is to let Mirri Code familiarize itself with the project:

```
Take a look at this project's directory structure and briefly describe what each directory is for.
```

Mirri Code automatically calls file-reading, search, and other tools to browse the relevant content before responding. Read-only operations are executed automatically by default without requiring confirmation. For operations that modify files or run shell commands, it asks for your confirmation before proceeding.

You can also describe a more concrete task directly:

```
Add a function in src/utils that converts any string to kebab-case, and add a unit test for it.
```

Mirri Code plans the steps, modifies the code, runs the tests, and tells you what it did at each step.

## Slash commands

Type `/` in the input box to list the commands available right now; the menu filters as you type. The ones you will reach for most:

| Command | Description |
| --- | --- |
| `/new` | Create a new session |
| `/compact` | Compact the session history to free up tokens |
| `/fork` | Fork this session into a new one |
| `/plan` | Toggle plan mode on/off |
| `/auto` | Auto-approve safe edits/reads |
| `/yolo` | Auto-approve everything (yolo mode) |
| `/status` | View session status |

## Where data is stored

Mirri Code stores its local data under `~/.mirri-code/` by default — config files, session records, logs, and the update cache. To move it elsewhere, point to a new path via the `MIRRICODE_HOME` environment variable. For the full directory layout, see [Data locations](../configuration/data-locations.md) and [Environment variables](../configuration/env-vars.md).

## Next steps

- [Interaction and input](./interaction.md) — input box operations, approval flow, Plan mode, and YOLO mode explained
- [Sessions and context](./sessions.md) — resuming sessions, compressing context, exporting sessions
- [Common use cases](./use-cases.md) — prompt examples for typical tasks
