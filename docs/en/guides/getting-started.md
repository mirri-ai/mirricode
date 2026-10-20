# Getting started

## What is Mirri Code

Mirri Code is an AI agent that runs in the terminal, helping you carry out software development tasks and day-to-day terminal operations — reading and modifying code, running shell commands, searching files, fetching web pages, and autonomously planning and adjusting its next steps based on feedback as it works.

It fits scenarios such as:

- **Writing and modifying code**: implementing new features, fixing bugs, completing refactors
- **Understanding a project**: exploring an unfamiliar codebase and answering questions about architecture and implementation
- **Automating tasks**: batch-processing files, running builds and tests, chaining multiple scripts together

The CLI is written in TypeScript. The official install script ships a self-contained native executable that requires no pre-installed runtime; the npm channel is retired (see the migration note below).

## Installation

Mirri Code's primary distribution is the desktop app; the CLI is also available via the official install script. The npm channel is retired and remains available to existing users only.

::: tip Before you install
Mirri Code is a fully interactive TUI application. For the best visual experience, run it in a terminal with true-color and ligature support, such as [Kitty](https://sw.kovidgoyal.net/kitty/) or [Ghostty](https://ghostty.org/).
:::

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

> On Windows, install [Git for Windows](https://gitforwindows.org/) before first launch. Mirri Code uses the bundled Git Bash as its shell environment; if Git Bash is installed in a custom location, set `MIRRICODE_SHELL_PATH` to the absolute path of `bash.exe`.

The script automatically downloads the latest release, verifies the checksum, and places the `mirri` executable on your `PATH`.

::: warning Note
The script locates GitHub Releases by `v`-prefixed tag. Until the first `vX.Y.Z` Release is published, resolving the latest version 404s — this is expected, use the desktop installer in the meantime. Pinning an old version (e.g. `1.10.0`) also 404s, because old releases use scoped tags and likely have no `manifest.json`.
:::

### Migrating from npm

The npm packages (`@mirri-ai/mirri-code`, `@mirri-ai/mirri-node`) are retired after their last release:

- **Existing installs keep working** — npm installations and runs are unaffected; no new versions are published, and `mirri upgrade` will not offer updates.
- **Move to the desktop app** — download the installer from [GitHub Releases](https://github.com/mirri-ai/mirricode/releases/latest), or use the [install script](#install-script) for the CLI. Configuration is stored in `~/.mirri-code/` (see [Where data is stored](#where-data-is-stored)) and carries over as-is.
- To uninstall the npm package: `npm uninstall -g @mirri-ai/mirri-code`.

## Upgrade and uninstall

After installation, verify that the executable is ready:

```sh
mirri --version
```

**Upgrade**: script-installed users run `mirri upgrade` — the CLI checks for updates and presents options. Note that the online version endpoint is frozen since npm publishing stopped, so `mirri upgrade` will not offer versions published to GitHub Releases — re-run the install script to overwrite with the latest release. Desktop users download the new installer from [GitHub Releases](https://github.com/mirri-ai/mirricode/releases/latest) and install over the old version.

**Uninstall**: if you installed via the script, delete the `mirri` executable (default: `~/.mirri-code/bin/mirri`). If you installed via npm:

```sh
npm uninstall -g @mirri-ai/mirri-code
```

## First launch

Move into your project directory and run `mirri` to start the interactive UI:

```sh
cd your-project
mirri
```

To run a single instruction without entering the interactive UI, use `-p`:

```sh
mirri -p "Take a look at this project's directory structure"
```

To resume the previous session, add `-c`:

```sh
mirri -c
```

On first launch you need to configure a model provider. In the interactive UI, enter `/provider` to open the provider manager:

```
/provider
```

Use `/provider` to add a provider and its API key (for example Mirri AI Open Platform), or a custom registry, and then select a default model. Run `/provider` again to view, remove, or re-configure providers and credentials.

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

::: tip Not sure what to do? Type `/help`
Type `/help` at any time to open the built-in command and keyboard shortcut panel. Use `↑`/`↓` to browse and `Esc` to close. To exit, type `/exit`, press `Ctrl-C` twice, or press `Ctrl-D` with the input box empty.
:::

## Common commands and keyboard shortcuts

For a first-time user, the following is all you need to know:

**Session commands**

| Command | Description |
| --- | --- |
| `/new` | Start a new session, clearing the current context |
| `/sessions` | Browse session history and choose one to resume |
| `/model` | Switch the current model |
| `/compact` | Manually compress the context to free up tokens |
| `/fork` | Fork the current session, keeping history but continuing independently |

**Most-used keyboard shortcuts**

| Shortcut | Description |
| --- | --- |
| `Esc` | Interrupt streaming output / close a popup |
| `Ctrl-C` | Interrupt output; press twice while idle to exit |
| `Shift-Tab` | Toggle Plan mode |
| `Ctrl-S` | Inject a message mid-stream without waiting for the current response to finish |
| `Ctrl-O` | Collapse / expand tool output and compaction summaries |

For the full list, type `/help` or visit [Slash commands reference](../reference/slash-commands.md) and [Keyboard shortcuts](../reference/keyboard.md).

## Where data is stored

Mirri Code stores its local data under `~/.mirri-code/` by default — config files, session records, logs, and the update cache. To move it elsewhere, point to a new path via the `MIRRICODE_HOME` environment variable. For the full directory layout, see [Data locations](../configuration/data-locations.md) and [Environment variables](../configuration/env-vars.md).

## Next steps

- [Interaction and input](./interaction.md) — input box operations, approval flow, Plan mode, and YOLO mode explained
- [Sessions and context](./sessions.md) — resuming sessions, compressing context, exporting sessions
- [Common use cases](./use-cases.md) — prompt examples for typical tasks
