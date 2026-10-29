# Scenarios

Each `journey-<NN>-<slug>.ts` file under this directory is a standalone
executable user-journey test (ADR-004 naming): it boots its own serve process
with an isolated temp home through the orchestrator (`src/proc.ts`), starts
its own in-process fake provider (`@mirri-ai/e2e-harness`), and asserts on
user-perceivable outcomes.

## Running

Requires the rust binary at `rust/target/debug/mirri` (build with
`cargo build -p mirri-cli --manifest-path rust/Cargo.toml`) or the
`MIRRICODE_E2E_RUST_BIN` env var pointing at one. No pre-started server and
no `MIRRICODE_SERVER_URL` is needed — scenarios manage their own processes.

```sh
# Single scenario (from the repo root)
npx tsx packages/server-e2e/scenarios/journey-01-first-launch.ts

# All journey scenarios (sequential; first failure exits non-zero)
pnpm --filter @mirri-ai/server-e2e test:scenarios
```

`test:scenarios` writes `reports/latest/index.html` with the scenario timeline,
including stdout/stderr milestones, HTTP request / response envelopes, and
WebSocket frames. Set `MIRRICODE_SERVER_E2E_REPORT_DIR=/tmp/server-e2e-report`
to write it somewhere else.

## Catalog

| File | What it does |
|---|---|
| `_template.ts` | Copy-paste starting point. No assertions; smoke-tests the lifecycle. |
| `journey-01-first-launch.ts` | R1 — fresh machine: boot serve with an empty home; healthz ready, home bootstrapped, backend v3, zero crashes. |
| `journey-02-first-configure.ts` | R2+R3 — first configure: empty-config guidance, then hot-reloaded model config makes the same session reply (no restart). |
| `journey-03-first-conversation.ts` | R4-R6 — read-only exploration, approval closed loop (file untouched before approval, changed after), and mid-stream interrupt with recovery. |
| `journey-04-real-task.ts` | R7+R8 — write-execute-report loop (edit file → run `node --test` → summarize real output) and subagent progress events. |
| `journey-05-return-visit.ts` | R9+R10 — cross-process return visit (kill → reboot → history intact, context reaches the model) and refresh continuity (re-subscribe from a cursor, no loss/duplication). |
| `journey-06-migrate.ts` | R11 — legacy-session fixture → `mirri migrate` → migrated session listed, history readable, legacy tree untouched. |
| `journey-07-multi-tab.ts` | R12 — two clients subscribed to one session observe the same event sequence. |

## Writing a new scenario

Copy `_template.ts` and fill in the TODO block.

Conventions:
- Exit `0` on pass; non-zero on any assertion failure or unhandled rejection.
- Always `try { ... } finally { stopServe }` — never leak serve processes.
- Print `▶` for milestones and `✓ / ✗` for the final outcome — `test:scenarios`
  surfaces those prefixes in CI logs.
- Default timeouts to 60s; tool-call scenarios may want 120s.
- Boot processes via `startServe` / stop via `stopServe` from `src/proc.ts` —
  never spawn `mirri` directly.
- Assert on user-perceivable outcomes (reply content, files, event sequences),
  never on wire frame shapes.
