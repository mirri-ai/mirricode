# server-e2e Agent Guide

This file contains package-local rules for `packages/server-e2e`.

## Testing Principle

- Journey scenarios assert on user-perceivable outcomes (reply content, files on disk, replay integrity) — never on protocol frame formats.
- Keep observability inside each scenario. Do not add a separate "observable" test or scenario as a substitute for making the existing cases explain what they drove and what the server returned.
- Every case should print structured, case-scoped details for the flow it exercises: key REST requests, response envelopes or unwrapped responses, WebSocket handshake / event milestones, prompt terminal states, and error envelopes.
- Keep logs factual and diagnostic. Print enough detail to debug the wire contract, but avoid unrelated narration.

## Workflow

- Scenarios are self-contained: each boots its own serve processes with isolated temp `MIRRICODE_HOME` dirs through the orchestrator (`src/proc.ts`) and its own in-process fake provider (`@mirri-ai/e2e-harness`). Never point a scenario at an externally pre-started server — there is no `MIRRICODE_SERVER_URL` prerequisite anymore.
- When adding or changing a scenario, update that scenario's observability at the same time.
- The rust binary resolves as `MIRRICODE_E2E_RUST_BIN` → `rust/target/debug/mirri` → `${CARGO_TARGET_DIR}/debug/mirri`. Build it locally with `cargo build -p mirri-cli --manifest-path rust/Cargo.toml` before running scenarios; docker builds it in-container.
- Run Docker e2e with `pnpm --filter @mirri-ai/server-e2e docker:e2e`; each run derives its Docker runner name/namespace from the current workspace to avoid cross-worktree conflicts.

## Command Reference

- Run all journey scenarios (self-managed processes; requires `rust/target/debug/mirri`): `pnpm --filter @mirri-ai/server-e2e test:scenarios`.
- Run a single scenario: `pnpm --filter @mirri-ai/server-e2e exec tsx scenarios/journey-01-first-launch.ts`.
- Run all server-e2e Vitest tests (self-managed processes): `pnpm --filter @mirri-ai/server-e2e test`.
- Run the full journey suite in docker: `pnpm --filter @mirri-ai/server-e2e docker:e2e`.
- Run type checking for this package: `pnpm --filter @mirri-ai/server-e2e typecheck`.
