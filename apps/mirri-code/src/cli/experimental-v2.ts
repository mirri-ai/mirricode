/**
 * Agent engine routing gates for the CLI surfaces.
 *
 * The agent-core-v2 engine is the DEFAULT for `mirri run-shell` (the
 * interactive TUI) and every other SDK-driven CLI surface. A truthy
 * `MIRRICODE_LEGACY_FLAG` re-enables the legacy agent-core (v1) backend.
 *
 * Note: `mirri web` always boots kap-server (the agent-core-v2 engine
 * server) — it does not consult this switch.
 */

import { createMirriHarness, type MirriHarness, type MirriHarnessOptions } from '@mirri-ai/mirri-code-sdk';
import { createMirriHarnessV2 } from '@mirri-ai/mirri-code-sdk';

export const MIRRICODE_LEGACY_ENV = 'MIRRICODE_LEGACY_FLAG';

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
 * Build the SDK harness for the configured engine. The v1 and v2 factories
 * share the same `MirriHarness` surface, so the TUI code path is unchanged —
 * only which backend backs it differs.
 */
export function createHarnessForEngine(options: MirriHarnessOptions): MirriHarness {
  return isV2Enabled()
    ? createMirriHarnessV2(options)
    : createMirriHarness(options);
}