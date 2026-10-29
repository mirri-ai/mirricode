# Repository-level Agent Guide

Reply in the same language as the user.

This is a monorepo for agent-assisted development. Keep the root `AGENTS.md` limited to hot-path rules: the project map, hard constraints, and workflow requirements — things every task needs to know.

`./rust/*` is the rust core implementation — the ONLY core (2026-10-28: the TypeScript engine chain and TS CLI/TUI shell are retired and removed; SDR-002 in `docs/specs/ts-v1-engine-retirement-requirements.md`). Every user-facing engine behavior change lands in `rust/crates/`. The TypeScript surface is limited to: the web/desktop frontends (`apps/mirri-web`, `apps/mirri-desktop`) and small support packages (`packages/protocol`, `packages/e2e-harness`, `packages/server-e2e`, `packages/release`). The `mirri` rust binary is the sole CLI entry point (`web` / `serve` / `acp` / `print` / `migrate` / `session-list` / `config-show` / `doctor`). see also `./rust/AGENTS.md`.

### Engine-target gate (spec-stage hard gate)

Before designing ANY change that touches engine/core behavior, answer FIRST: "which engine does the user actually run?" (all surfaces → rust). The default and only landing target is the rust core. There is no TS engine to fall back to.

## Working Principles

- Think from first principles. Start from real requirements, code facts, and verification results; if the goal is unclear, discuss it with the user first.
- Treat code, not documentation, as the source of truth. Unless the user explicitly says otherwise, do not read ordinary Markdown just to understand the implementation.
- Before making code changes, read the relevant code and the most recent constraints, and follow the nearest `AGENTS.md` in the directory tree.
- Keep changes focused. Do not slip in unrelated refactors along the way.
- When committing, do not add any co-author attribution, and do not reveal the identity of the agent in commit messages, PR descriptions, or any explanatory text.

## Project Map

- `apps/mirri-web`: the browser web UI. Vue 3 + Vite + vue-i18n; talks to the server over REST + WebSocket under `/api/v1` (wire types re-implemented locally). The dev proxy targets the **rust core engine (v3)** by default (root `pnpm dev:web`). See `apps/mirri-web/AGENTS.md`.
- `apps/mirri-desktop`: the Electron desktop shell. Packages and spawns the rust `mirri web` binary as its backend.
- `apps/mirri-desktop-lite`: the experimental Electrobun lightweight desktop shell (second shell, coexists with `apps/mirri-desktop`). Own daemon (port 55828, `~/.mirri-code/server-lite/`); implements the web bridge contract; **excluded from the pnpm workspace and root tsconfig** (Hutch-managed toolchain — run `hutch install` inside the app, not pnpm). See `apps/mirri-desktop-lite/README.md`.
- `packages/protocol`: wire-protocol pure functions and contract schemas shared with the web front end and the black-box e2e client.
- `packages/e2e-harness`: e2e test infrastructure — the fake LLM provider server (OpenAI-compatible SSE) used by the web rust e2e suites.
- `packages/server-e2e`: live black-box e2e wire client and scenarios against a running rust server (`MIRRICODE_SERVER_URL`, default `http://127.0.0.1:58627`). See `packages/server-e2e/AGENTS.md`.
- `packages/release`: the version anchor (`@mirri-ai/release`, private) that all changesets target.

## Environment Requirements

- **Node.js**: `>=24.15.0` (from the root `package.json` `engines`; `.nvmrc` is `24.15.0`, used by nvm / fnm / mise to pick the minimum recommended version).
- **pnpm**: `10.33.0` (from the root `package.json` `packageManager`).
- `pnpm install` will fail when the Node version is not satisfied, because `.npmrc` sets `engine-strict=true`.

## Monorepo Workspace Maintenance

- `pnpm-workspace.yaml` is the source of truth for workspace membership, but `flake.nix` also contains **hardcoded** `workspacePaths` and `workspaceNames` lists.
- **Whenever you add or remove a workspace package, you MUST update both `pnpm-workspace.yaml` and `flake.nix` — for every package, including leaf / test / e2e packages that nothing depends on.**
  - `pnpm-workspace.yaml` uses globs (`packages/*`, `apps/*`), so most packages land there automatically; `flake.nix` is fully manual and is where omissions happen.
  - Missing a path in `flake.nix`'s `workspacePaths` will silently drop files from the Nix build's `src` fileset.
  - Missing a name in `flake.nix`'s `workspaceNames` will break `pnpmConfigHook` because dependencies for that workspace will not be fetched.
- The automated "Check flake.nix workspace sync" (`scripts/check-nix-workspace.mjs`) only validates the transitive dependency **closure of its anchor package** (`@mirri-ai/mirri-desktop`). A leaf package outside that closure (e.g. an e2e package nobody imports) slips through even when it is missing from `flake.nix`. A green check is therefore NOT proof that `flake.nix` is fully in sync — keep it updated by hand on every add/remove, do not rely on the check to catch omissions.

## General Coding Rules

- For optional object properties, pass `undefined` directly instead of using conditional spread.
  - YES: `{ user }`
  - NO: `{ ...(user ? { user } : undefined) }`
- Optional object properties do not need to additionally allow `undefined` in the type.
  - YES: `interface Options { user?: User }`
  - NO: `interface Options { user?: User | undefined }`
- Internal methods with only a single parameter should not be turned into options objects just for stylistic uniformity.
- Except for a package's `index.ts`, other `index.ts` files should prefer `export * from './module';`.
- Do not add too many new test files. Prefer adding tests to the existing test file of the corresponding component or module.
- When a test fails because of a user modification, default to fixing the test first; do not change the implementation to satisfy an old test unless the implementation truly has a bug.
- Do not sacrifice code quality for external compatibility unless the user explicitly asks for it. Breaking changes go through changesets and a `major` bump, gated by the rule below.

### Self-Check Before Committing

Before committing code changes, you MUST verify that both typecheck and tests pass for every package you modified. Run the affected package's typecheck and test commands, and confirm they succeed:

```bash
# Example: after changing the web app
pnpm --filter @mirri-ai/mirri-web run typecheck
pnpm --filter @mirri-ai/mirri-web exec vitest run test/path/to/affected.test.ts
```

If either fails, fix the issue before committing. Do not commit broken code expecting the CI to catch it.

### Test Case Naming

Use **Given-When-Then** style for test names. Each `it()` description should read as a sentence stating the expected behavior under specific conditions:

```typescript
// Good: Given-When-Then — states behavior under conditions
it('should return 401 when user is not authenticated', () => { });
it('should send email when order is completed', () => { });

// Bad: mentions implementation details, class names, or internal structure
it('calls augmentToolsForCapabilities with correct args', () => { });
```

Prefer `should ... when ...` phrasing. Avoid mentioning variable names, function names, class names, or internal implementation details in the test description.

## Where to Update Instructions

- Hard rules that affect almost every task: update the root `AGENTS.md`.
- Rules that only affect a specific directory: update the nearest sub-directory `AGENTS.md`.
- Keep instruction updates focused and supported by code facts.

## CI Verification Requirements

The CI pipeline has checks that are NOT covered by the local `build.sh` quality gate. When your changes touch the areas below, you MUST verify them before pushing:

- **Nix build** (`nix build .#mirri-code`): Required when modifying:
  - `flake.nix` (fileset, workspace lists, build logic). The package builds the **rust** `mirri` binary (`mirri-cli` with the SPA embedded via rust-embed) and imports `rust/Cargo.lock` with `importCargoLock`, so no vendor hash is maintained by hand; the JS toolchain only builds `apps/mirri-web`'s SPA.
  - `rust/scripts/release-package.mjs` (resolves `yazl` from the root `package.json`)
  - Any package that appears in `flake.nix`'s `workspaceNames`
  - Nix is **not** a prerequisite; a `pnpm-lock.yaml` / workspace-list change also needs the `pnpmDeps` hash refreshed — see the `nix-pnpm-deps-hash` skill.

- **PR title check**: PR titles must follow Conventional Commit style. The `pr-title-checker` workflow validates this on every PR.

- **Test scope**: The root `pnpm test` runs the maintained TypeScript suites — `apps/mirri-web` and `apps/mirri-desktop`. The default run sets `MIRRICODE_E2E_SKIP_RUST=1`: the web e2e suites that drive a real engine binary would otherwise run `cargo build -p mirri-cli` and blow the hook timeout. Run `pnpm test:e2e` (after building `rust/target/debug/mirri`) to exercise them.

- **Rust workspace test/clippy runtime** (measured 2026-10-10, dev workstation, warm incremental build cache): `cargo test --manifest-path rust/Cargo.toml --workspace` takes ~7 minutes wall-clock (~6m44s, mostly compilation; the test bodies themselves finish in seconds), and a follow-up `cargo clippy --workspace --all-targets -- -D warnings` adds ~45 s. A cold cache (`cargo clean`, CI, or many changed crates) is significantly longer. Always run these as background tasks with a timeout of at least 20 minutes; foreground shell calls cap at 300 s and will be killed mid-build. CI runs the same suite and clippy in the `rust-test` job (`.github/workflows/ci.yml`), so a red rust test blocks a PR.

## Release Pipeline (desktop-first)

- Version source of truth: `packages/release` (`@mirri-ai/release`, private). All changesets target it; `rust/scripts/sync-version.mjs` fans its version out to the rust workspace and desktop. Never bump rust versions by hand — run the sync script. `pnpm run version:release` (used by the changesets action) already chains `changeset version` + the sync script, so the generated release PR carries the rust workspace, desktop and `Cargo.lock` versions.
- Releasing = merging the changesets release PR (`ci: release packages`). CI detects the anchor version change, creates GitHub Release `vX.Y.Z`, and uploads desktop installers + rust CLI zips (ACP ships inside the `mirri` CLI as the `mirri acp` subcommand) + `manifest.json` to it.
- npm publishing is retired (`@mirri-ai/mirri-code`, `@mirri-ai/mirri-node` are frozen at their last published versions; the `latest.json` version endpoint is frozen — never point it at the anchor). Do not add `publish` scripts or npm-publish workflows.
- Any user-perceivable change (rust core, desktop, web, CLI) gets a changeset against the anchor: `minor` for features, `patch` for fixes.

## Code Exploration

The project is indexed to a code knowledge graph (codebase-memory-mcp). Prefer the `mcp__codebase_memory_mcp__*` tools over raw grep/find when exploring code structure:

| Scenario | Tool | Why |
|----------|------|-----|
| Understand how a feature works | `search_graph` + `get_code_snippet` | Returns symbol-level relationships, not just text matches |
| Find callers / callees of a function | `trace_path` | Full call-chain traversal in one call |
| Understand project architecture | `get_architecture` | Packages, entry points, hotspots, clusters |
| Analyze impact of a change | `trace_path` + `query_graph` | Blast radius with dependency depth |
| Find related code by semantics | `search_graph(semantic_query=[...])` | Bridges vocabulary (finds "publish" when you search "send") |
| Complex multi-hop queries | `query_graph` | Cypher-like graph queries |

**Still use Grep/Glob/Read** for: exact string/variable search, file path lookup, reading specific file contents, and pre-edit line-level pinpointing.

**Workflow**: explore with `get_architecture` first, locate with `search_graph`, trace with `trace_path`, then drill into source with `get_code_snippet` or `Read`.

## Workflow Requirements

- Prefer `rg` / `rg --files` when reading code.
- Internal product requirement/design documents live under `docs/specs/` and follow its governance (`docs/specs/AGENTS.md`). In short: spec documents hold the requirement/design itself only — no progress or implementation status; current-state notes and pain points go in the domain's `{domain}-context.md`; status/progress tracking goes in the domain's `{domain}-ROADMAP.md` (or directory-level `ROADMAP.md`) and plan files, never mixed into specs; requirement docs are written in non-technical product language (PM-readable), with technical shapes (interface names, signatures, mechanisms) belonging in `{domain}-architecture.md`; requirement definitions must pass the R1–R9 definition review checklist in `docs/specs/AGENTS.md`; requirement/design changes must be written back into the spec itself as decision records (SDR for requirements, ADR for design), not just in a roadmap or commit message.
- When designing changes, follow existing boundaries and local patterns first.
- In public text and test data, replace real internal identifiers with neutral placeholders such as `example.com`, `example.test`, and `YOUR_API_KEY`. Before opening a PR, ask a read-only agent to audit the diff for context-specific internal identifiers.
- When creating a PR, the PR title must follow Conventional Commit style, e.g. `chore: remove legacy format commands`.
- When an AI agent opens or updates a PR, fill in `.github/pull_request_template.md` — link the related issue or explain the problem, then describe what changed. Do not leave placeholder text or submit a generic summary of the diff.
- Do not submit vague AI-generated PR text. The human author must understand the change well enough to explain the code, edge cases, and why the approach fits this repository.
- After finishing a task and before submitting a PR, you must run the `gen-changesets` skill (see `.agents/skills/gen-changesets/SKILL.md`) and generate a changeset under `.changeset/` according to its rules.
- When generating a changeset, **never** decide on a `major` bump on your own. When you judge a change to meet the major criteria (breaking changes, incompatible user configuration, renamed or removed commands/arguments, changed behavior semantics, etc.), you must stop and explain it to the user and ask for confirmation. **Only write `major` after the user has explicitly agreed.** Otherwise default to `minor` (and fall back to `patch` if `minor` is unclear). See the "Hard rule: confirm with the user before writing `major`" section in `.agents/skills/gen-changesets/SKILL.md` for details.
- Prefer importing via `import ... from '#/...'`, which serves the same purpose as `import ... from '@/...'`.
- Do not commit throwaway scratch or exploratory files. Never stage:
  - Agent working notes or handoff/summary documents (e.g. `HANDOVER-*.md`, `HANDOFF-*.md`, `handoff.md`).
  - Throwaway UI/UX prototypes or design mockups (e.g. `*-designs.html`, `*-mockup.html`, `*-demo(s).html`) at the repo root or under a `design/` folder. The only tracked `.html` files should be Vite `index.html` entrypoints.
  Before committing or opening a PR, run `git status` and `git diff --staged --stat` and remove anything matching these patterns. Put scratch work under `.tmp/` (gitignored) instead of the repo root or the source tree.
