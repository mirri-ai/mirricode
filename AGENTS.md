# Repository-level Agent Guide

Reply in the same language as the user.

This is a TypeScript monorepo built for agent-assisted development. Keep the root `AGENTS.md` limited to hot-path rules: the project map, hard constraints, and workflow requirements — things every task needs to know.

## Working Principles

- Think from first principles. Start from real requirements, code facts, and verification results; if the goal is unclear, discuss it with the user first.
- Treat code, not documentation, as the source of truth. Unless the user explicitly says otherwise, do not read ordinary Markdown just to understand the implementation.
- Before making code changes, read the relevant code and the most recent constraints, and follow the nearest `AGENTS.md` in the directory tree.
- Keep changes focused. Do not slip in unrelated refactors along the way.
- When committing, do not add any co-author attribution, and do not reveal the identity of the agent in commit messages, PR descriptions, or any explanatory text.

## Project Map

- `apps/mirri-code`: the CLI / TUI application. It consumes core capabilities through `@mirri-ai/mirri-code-sdk` and must not depend directly on `@mirri-ai/agent-core`. When writing or modifying its terminal UI, use the `write-tui` skill (`.agents/skills/write-tui/SKILL.md`).
- `apps/mirri-web`: the browser web UI, a peer to the TUI. Vue 3 + Vite + vue-i18n; talks to the server over REST + WebSocket under `/api/v1`. It must not depend on `@mirri-ai/agent-core` (wire types are re-implemented locally). See `apps/mirri-web/AGENTS.md`.
- `apps/vis`, `apps/vis/server`, `apps/vis/web`: visual debugging tools for sessions and replays.
- `packages/agent-core`: the unified agent engine, including Agent, Session, profile, skills, tools, plan, permission, background, records, the in-process DI service layer (`src/services/`), and other core capabilities.
- `packages/node-sdk`: the public TypeScript SDK and harness.
- `packages/kosong`: the LLM / provider abstraction layer.
- `packages/kaos`: the execution environment and file/process abstractions.
- `packages/oauth`: Mirri OAuth and managed auth utilities.
- `packages/telemetry`: shared client-side telemetry infrastructure.
- `packages/server`: the Mirri Code server. Hosts `agent-core` sessions and exposes them over REST + WebSocket (`/api/v1`); bootstrapped from `src/start.ts` and consumed by `apps/mirri-code`. See `packages/server/AGENTS.md`.
- `packages/server-e2e`: live e2e tests and scenarios against a running server (`MIRRICODE_SERVER_URL`, default `http://127.0.0.1:58627`). See `packages/server-e2e/AGENTS.md`.

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
- The automated "Check flake.nix workspace sync" (`scripts/check-nix-workspace.mjs`) only validates the transitive dependency **closure of `@mirri-ai/mirri-code`**. A leaf package outside that closure (e.g. an e2e package nobody imports) slips through even when it is missing from `flake.nix`. A green check is therefore NOT proof that `flake.nix` is fully in sync — keep it updated by hand on every add/remove, do not rely on the check to catch omissions.

## General Coding Rules

- For optional object properties, pass `undefined` directly instead of using conditional spread.
  - YES: `{ user }`
  - NO: `{ ...(user ? { user } : undefined) }`
- Optional object properties do not need to additionally allow `undefined` in the type.
  - YES: `interface Options { user?: User }`
  - NO: `interface Options { user?: User | undefined }`
- Internal methods with only a single parameter should not be turned into options objects just for stylistic uniformity.
- Except for a package's `index.ts`, other `index.ts` files should prefer `export * from './module';`.
- The `Agent` class in `packages/agent-core/src/agent` must be usable on its own. The constructor must not force the caller to create a `Session` instance, nor require an `agentId` or `session`. It may accept an optional `sessionId` as a request-config hint — for example mapped to the provider's `prompt_cache_key` — but the instance must not hold `sessionId`, and must not depend on the Session lifecycle, metadata, or parent/child relationship logic.
- Do not add too many new test files. Prefer adding tests to the existing test file of the corresponding component or module.
- When a test fails because of a user modification, default to fixing the test first; do not change the implementation to satisfy an old test unless the implementation truly has a bug.
- Do not sacrifice code quality for external compatibility unless the user explicitly asks for it. Breaking changes go through changesets and a `major` bump, gated by the rule below.

### Self-Check Before Committing

Before committing code changes, you MUST verify that both typecheck and tests pass for every package you modified. Run the affected package's typecheck and test commands, and confirm they succeed:

```bash
# Example: after changing agent-core
pnpm --filter @mirri-ai/agent-core run typecheck
pnpm --filter @mirri-ai/agent-core exec vitest run test/path/to/affected.test.ts
```

If either fails, fix the issue before committing. Do not commit broken code expecting the CI to catch it.

### Test Case Naming

Use **Given-When-Then** style for test names. Each `it()` description should read as a sentence stating the expected behavior under specific conditions:

```typescript
// Good: Given-When-Then — states behavior under conditions
it('should return 401 when user is not authenticated', () => { });
it('should send email when order is completed', () => { });
it('should inject MCP tools when profile declares capabilitiesRequired', () => { });
it('should inherit parent capabilities when child does not override', () => { });

// Bad: mentions implementation details, class names, or internal structure
it('does not declare capabilitiesRequired (uses mcp__* instead)', () => { });
it('calls augmentToolsForCapabilities with correct args', () => { });
it('CapabilityRegistry registers builtin tools', () => { });
```

Prefer `should ... when ...` phrasing. Avoid mentioning variable names, function names, class names, or internal implementation details in the test description.

## Experimental Features

- Gate a not-yet-public feature behind an experimental flag. Add the flag to the registry at `packages/agent-core/src/flags/registry.ts`, then check it with `flags.enabled('my-feature')`. Flags are env-driven and default off: `MIRRICODE_EXPERIMENTAL_<NAME>` toggles one, `MIRRICODE_EXPERIMENTAL_FLAG` enables all. Release by flipping the entry's `default` to `true`.

## Where to Update Instructions

- Hard rules that affect almost every task: update the root `AGENTS.md`.
- Rules that only affect a specific directory: update the nearest sub-directory `AGENTS.md`.
- Keep instruction updates focused and supported by code facts.

## CI Verification Requirements

The CI pipeline has checks that are NOT covered by the local `build.sh` quality gate. When your changes touch the areas below, you MUST verify them before pushing:

- **Nix build** (`nix build .#mirri-code`): Required when modifying:
  - `flake.nix` (fileset, workspace lists, build logic)
  - `apps/mirri-code/scripts/native/` (native build scripts)
  - `apps/mirri-code/package.json` (build scripts, dependencies)
  - Any package that appears in `flake.nix`'s `workspaceNames`
  - If you cannot run `nix build` locally (no Nix installed), at minimum verify that all files referenced in `flake.nix`'s fileset exist in the repo, and that package names in `scripts/native/native-deps.mjs` match the actual npm scope (`@mirri-ai/*`, not `@moonshot-ai/*`).

- **PR title check**: PR titles must follow Conventional Commit style. The `pr-title-checker` workflow validates this on every PR.

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
