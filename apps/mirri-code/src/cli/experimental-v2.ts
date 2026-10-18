/**
 * Agent engine routing gates for the CLI surfaces.
 *
 * The agent-core-v2 engine is the DEFAULT for `mirri run-shell` (the
 * interactive TUI) and every other SDK-driven CLI surface. A truthy
 * `MIRRICODE_LEGACY_FLAG` re-enables the legacy agent-core (v1) backend.
 * A truthy `MIRRICODE_RUST_ENGINE` selects the Rust NAPI engine
 * (`@mirri-ai/mirri-node` → `SDKRpcClientRust`) and wins over both TS
 * backends.
 *
 * Note: `mirri web` boots kap-server (the agent-core-v2 engine server) by
 * default; a truthy `MIRRICODE_RUST_ENGINE` makes it spawn the Rust `mirri
 * serve` binary instead (see `cli/sub/web/run.ts`).
 */

import {
  createMirriHarness,
  createMirriHarnessRust,
  createMirriHarnessV2,
  type MirriHarness,
  type MirriHarnessOptions,
} from '@mirri-ai/mirri-code-sdk';

export const MIRRICODE_LEGACY_ENV = 'MIRRICODE_LEGACY_FLAG';
export const MIRRICODE_RUST_ENGINE_ENV = 'MIRRICODE_RUST_ENGINE';

const TRUTHY_VALUES = new Set(['1', 'true', 'yes', 'on']);

function isTruthyEnv(
  key: string,
  env: Readonly<Record<string, string | undefined>>,
): boolean {
  return TRUTHY_VALUES.has((env[key] ?? '').trim().toLowerCase());
}

/** Truthy `MIRRICODE_LEGACY_FLAG` selects the legacy (v1) engine path. */
export function isLegacyEnabled(
  env: Readonly<Record<string, string | undefined>> = process.env,
): boolean {
  return isTruthyEnv(MIRRICODE_LEGACY_ENV, env);
}

/** The agent-core-v2 engine path is the default. */
export function isV2Enabled(
  env: Readonly<Record<string, string | undefined>> = process.env,
): boolean {
  return !isLegacyEnabled(env);
}

/**
 * Truthy `MIRRICODE_RUST_ENGINE` selects the Rust NAPI engine path
 * (`SDKRpcClientRust`, backed by the native `@mirri-ai/mirri-node` binding).
 * The binding may not be built for the current platform; callers that pick
 * this path get the client's loud per-call `not_implemented` error when it
 * is unavailable.
 */
export function isRustEngineEnabled(
  env: Readonly<Record<string, string | undefined>> = process.env,
): boolean {
  return isTruthyEnv(MIRRICODE_RUST_ENGINE_ENV, env);
}

/**
 * Build the SDK harness for the configured engine. The v1, v2, and Rust
 * factories share the same `MirriHarness` surface, so the TUI code path is
 * unchanged — only which backend backs it differs. Precedence: Rust
 * (`MIRRICODE_RUST_ENGINE`) > v2 (default) > v1 (`MIRRICODE_LEGACY_FLAG`).
 */
export function createHarnessForEngine(options: MirriHarnessOptions): MirriHarness {
  if (isRustEngineEnabled()) {
    return createMirriHarnessRust(options);
  }
  return isV2Enabled()
    ? createMirriHarnessV2(options)
    : createMirriHarness(options);
}