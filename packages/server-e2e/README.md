# @mirri-ai/server-e2e

Journey-level E2E suite for the mirri-code server: user-journey scenarios
(R1–R12, see `docs/specs/server-e2e-requirements.md`) driven against a REAL
`mirri` rust binary. This package is **private** — internal tooling only.

## When to use this

- You want to verify a user-visible journey end to end — first launch, first
  configure, first conversation (tool calls / approvals / interrupt), a real
  task loop with a subagent, return visits across process restarts, legacy
  migration, multi-tab consistency — with the assertions on user-perceivable
  outcomes (reply content, files on disk, replay integrity).
- You want the wire-level reference for the REST + WS surface the web client
  consumes (unified JSON-RPC endpoint, event notifications, reverse-RPC
  approvals) — while keeping assertions journey-level, not frame-level.
- You changed rust engine behavior that affects any of those journeys and
  need the fastest full-journey regression signal.

## When NOT to use this

- You're testing the WS gateway or REST handlers in isolation — keep those in
  the rust crates' own test suites (faster, asserts internals directly).
- You want a typed in-process facade for user-facing code.

## How journeys work

Each `scenarios/journey-<NN>-<slug>.ts` file is a standalone executable
scenario (ADR-002/004 naming). A scenario:

1. boots its own `mirri serve` process through the orchestrator
   (`src/proc.ts`) with an isolated temp `MIRRICODE_HOME` — the "fresh
   machine" state, zero sharing between cases;
2. starts its own in-process fake LLM provider (`@mirri-ai/e2e-harness`)
   scripted per turn — no host-side provider or pre-started server needed;
3. asserts only user-perceivable outcomes and exits 0/1.

The runner (`scripts/run-scenarios.ts`) executes every `journey-*.ts` file
sequentially; first failure exits non-zero.

## Running

Local (requires a built rust binary at `rust/target/debug/mirri` — build with
`cargo build -p mirri-cli --manifest-path rust/Cargo.toml`):

```sh
pnpm --filter @mirri-ai/server-e2e test:scenarios
```

Docker (builds the binary inside the container; no local rust toolchain
needed):

```sh
pnpm --filter @mirri-ai/server-e2e docker:e2e
```

Unit/self tests (vitest — process-orchestrator and ws-client cases; they boot
their own real serve processes, no prerequisites):

```sh
pnpm --filter @mirri-ai/server-e2e test
```

Every run writes a browser-readable report to
`packages/server-e2e/reports/latest/index.html` (override with
`MIRRICODE_SERVER_E2E_REPORT_DIR`): case-scoped milestones, HTTP request /
response envelopes, WebSocket frames, and per-case results, with JSON payloads
collapsed so the terminal stays concise.

The binary the orchestrator spawns resolves in this order: the
`MIRRICODE_E2E_RUST_BIN` env var → `rust/target/debug/mirri` (local repo) →
`${CARGO_TARGET_DIR}/debug/mirri` (docker container builds).

## Old-scenario disposition

The original 12 numbered wire-level scenarios (pre-journey suite) were retired
when the journey suite landed. Their disposition:

| Old scenario | Disposition |
|---|---|
| `01-create-and-send` | Covered by `journey-02-first-configure` |
| `02-tool-call-with-approval` | Covered by `journey-03-first-conversation` |
| `03-refresh-replay` | Covered by `journey-05-return-visit` |
| `04-stateless-controls` | Deleted — depended on a TS debug-injection endpoint with no rust equivalent |
| `05-workspace` | Deleted — workspace registry interface domain, not a user journey surface |
| `06-model-catalog` | Deleted — model catalog interface domain, not a user journey surface |
| `07-session-children` | Deleted — session-tree engine semantics, not a user journey surface |
| `08-pending-recovery` | Covered by `journey-05-return-visit` (restart recovery) |
| `09-image-file-prompts` | Open item — multimodal input journey; the fake provider cannot express multimodal model semantics (needs a real-model tiered plan, see `docs/specs/server-e2e-ROADMAP.md`) |
| `10-prompt-queue-steer` | Deleted — prompt-queue engine semantics, not a user journey surface |
| `11-terminal` | Covered by `journey-04-real-task` (command execution loop) |
| `12-send-and-cancel` | Deleted — scheduler engine semantics, not a user journey surface |

## Docker workflow

`docker:e2e` builds `mirri-server-e2e-base` from the (gitignored, local) root
`Dockerfile`, layers `packages/server-e2e/Dockerfile` on top, then runs a
one-shot container. Inside the container it installs pnpm deps, builds the
rust binary into the writable `/data/rust-target` volume layer (the repo is
mounted read-only), exports `MIRRICODE_E2E_RUST_BIN` to point the orchestrator
at it, and runs `pnpm test:scenarios`. No host port is published and no server
is pre-started — journey scenarios manage their own processes.

Workspace `node_modules` are mounted from a per-run host cache
(`~/.mirricode-server-dev/docker-e2e/<run-id>/nm`) so repeated runs skip
reinstalling. `<run-id>` is deterministic per worktree
(`<repo-basename>-<cksum-of-repo-path>`), so parallel worktrees do not collide.
Override with `MIRRICODE_SERVER_E2E_RUN_ID`, `MIRRICODE_SERVER_E2E_STATE_ROOT`,
or `MIRRICODE_SERVER_E2E_REPORT_DIR_HOST`. Expect the in-container rust build
to take a few minutes on a cold cache.

## Public API summary

| Symbol | Purpose |
|---|---|
| `DaemonClient` | Main facade — REST + unified JSON-RPC WS, reverse-RPC approval/question handlers. |
| `HttpClient` | REST helpers only (no WS). |
| `WsRpcClient` | Raw unified JSON-RPC WS client — handshake, `call`, subscribe, notifications. |
| `startServe` / `stopServe` / `runOneShot` | Process orchestrator (ADR-002) — spawn/kill/graceful-stop real `mirri` processes with isolated homes. |
| `fakeProviderConfigToml` | Emits a valid `config.toml` pointing `default_model` at the fake provider. |
| `EnvelopeError` | Thrown by `unwrap()` / HTTP helpers when `envelope.code !== 0`. |
| `fetchWithReport` / `writeHtmlReport` | Capture direct fetch calls and render the JSONL trace as a single HTML report. |

See `scenarios/README.md` for the scenario catalog and conventions.

## Scope notes

- **Self-managed processes only** — scenarios never point at an externally
  pre-started server; the orchestrator is the only place process birth/death
  happens.
- **Not published** — `private: true`. Internal tooling only.
