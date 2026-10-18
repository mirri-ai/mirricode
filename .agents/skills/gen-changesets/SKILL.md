---
name: gen-changesets
description: Use when generating changesets in the mirri-code repository — selecting the @mirri-ai/release version anchor, judging what is user-perceivable, choosing a bump level, confirming major bumps, and writing English changelog entries.
---

# Generate Changesets

`mirri-code` uses changesets for exactly two outputs: it moves the **product version anchor** and writes the user-facing changelog that ships with a GitHub Release. npm publishing is retired — no package in this repository is published to a registry.

## The Anchor Package

Every changeset targets one package, and only that one:

```markdown
---
"@mirri-ai/release": minor
---

Add the `mirri doctor` subcommand to validate `config.toml`. Run `mirri doctor` to check it.
```

`@mirri-ai/release` lives in `packages/release`. It is private, never built, never published, and holds only the product version plus `CHANGELOG.md`. That version becomes the release tag `vX.Y.Z`, and `rust/scripts/sync-version.mjs` fans it out to the rust workspace and the desktop app.

Rules:

- **Never list any other package.** Not `@mirri-ai/mirri-code`, not `@mirri-ai/mirri-code-sdk`, not any workspace package, not `@mirri-ai/pi-tui`.
- **Never put two packages in one frontmatter.** A second package entry is a mistake, not a feature.
- **Never bump a version by hand.** Do not edit `packages/release/package.json`, the rust workspace versions, or the desktop version. Run `node rust/scripts/sync-version.mjs` instead; CI runs it with `--check`.
- **Do not add changesets for internal-only work.** See the table below.

## What Needs a Changeset

Judge at the product level. The shipped product is the rust engine binary, the desktop app, the web UI, and the CLI. If a user of any of those can perceive the change, it needs a changeset.

| Change | Changeset |
| --- | --- |
| New user-facing feature (rust core, desktop, web, CLI) | `minor` on `@mirri-ai/release` |
| Bug fix a user can observe | `patch` |
| Behavior change, renamed or removed command / flag / config key | `major` — only with explicit user approval |
| Internal refactor, module split, dependency cleanup, IO plumbing, renamed internal types | none |
| Test-only, docs-only, CI-only, build-tooling-only | none |
| Changes under `plugins/` | none — each plugin carries its own version in `mirri.plugin.json` / `plugins/marketplace.json` and ships via the marketplace CDN |
| New feature implemented in the frozen TypeScript core (`packages/*`, `apps/mirri-code`) | none — this is out of policy; engine features belong in `rust/crates/` |

The TypeScript core is frozen. A change under `packages/*` or `apps/mirri-code` gets a changeset only when it still reaches a user — for example a wire-contract fix that changes what the web UI or an SDK consumer sees. Refactors and internal fixes there do not get one.

## Bump Levels

| Level | When to use |
| --- | --- |
| `patch` | A user-observable fix, or a small improvement to an existing feature |
| `minor` | A substantial new capability a user could not do before |
| `major` | Breaking: incompatible config changes, removed or renamed commands / arguments / config keys, changed behavior semantics |

When in doubt between `patch` and `minor`, pick `patch` if it improves something that already exists, and `minor` only when it unlocks new capability.

### Major Rule

Never write `major` on your own.

If a change looks major, stop, explain why, and ask the user for explicit approval. Only write `major` after they agree. If the user does not reply, replies ambiguously, or disagrees, fall back to `minor`; if `minor` is also unclear, fall back to `patch`.

An existing `major` changeset in the tree is not permission to write another one — the approval is per change.

## Granularity

One entry per user-facing theme, not one entry per internal commit. A feature that took fifteen commits across rust, server and web deserves one entry that says what the user gets.

Keep unrelated themes apart. Broad consolidation is fine when the entries still read as distinct product changes; do not merge a UI redesign and a provider addition into one paragraph just to save a file.

Name the file with a short kebab-case slug, e.g. `mirri-doctor-cli.md` or `web-terminal-panel.md`.

## Wording Rules

- Entries are written in **English**, at product level, for someone who uses the product rather than someone who reads the diff.
- Lead with what changed for the user in one short sentence. At most add a one-line usage hint for a new feature.
- Prefix web-UI-only entries with `web: ` — for example `web: Add a terminal panel to the chat workspace.` The docs changelog sync relies on this prefix.
- Name CLI commands, flags and config keys when the user types them (`mirri doctor`, `--no-open`, `disabled_skills`). Do not name internal RPC methods, structs, classes, files or modules.
- Do not mention PR numbers, commit hashes, issue numbers, or the agent/author identity.
- Do not use vague filler: `refactor`, `optimize`, `improve`, `enhance`, `various fixes`. Say what actually changed.
- Do not overstate. If only half a theme landed, describe the half that landed.
- Use neutral placeholders for any example value: `example.com`, `example.test`, `YOUR_API_KEY`.

## Examples

A new CLI subcommand:

```markdown
---
"@mirri-ai/release": minor
---

Add the `mirri doctor` subcommand to validate `config.toml` against the engine's own parser. Run `mirri doctor` to check the current config.
```

A web-only feature:

```markdown
---
"@mirri-ai/release": minor
---

web: Add a background tasks overview with run-in-background actions, opened from settings or the sidebar.
```

A user-visible fix:

```markdown
---
"@mirri-ai/release": patch
---

Fix background task notifications being re-delivered after a restart, which woke the agent repeatedly and wasted tokens.
```

An internal change — no changeset at all:

```text
Split the engine config module into focused submodules.
```

That last one is a red flag if it appears in a changeset: module splits, dependency removals, IO injection and renamed internals are invisible to users and belong in the commit message, not the changelog.

## Verify

After writing or editing changesets, confirm they parse and that the aggregate bump is what you intended:

```sh
pnpm changeset status
```

It prints the packages to be bumped at each level. If it reports anything other than `@mirri-ai/release` at the level you meant, fix the frontmatter before committing.

## Red Flags

- A `major` written without explicit user approval.
- A package other than `@mirri-ai/release` in the frontmatter.
- Two packages in one changeset file.
- A changelog entry in Chinese, or one that reads like a commit message.
- An entry that names internal files, classes, RPC methods or modules.
- A `web:` entry missing the prefix.
- An internal-only change carrying a changeset.
- A changeset for `plugins/` work.
- A hand-edited version in `packages/release/package.json`, the rust workspace, or the desktop app.
- An entry claiming more than the diff actually did.
