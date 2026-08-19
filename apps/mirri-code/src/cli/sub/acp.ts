/**
 * `mirri acp` sub-command routing.
 *
 * By default the command delegates to the agent-core-v2 ACP server
 * (`@mirri-ai/acp-server`), loaded lazily so parsing the CLI does not
 * initialize the engine. A truthy `MIRRICODE_LEGACY_FLAG` keeps the SDK
 * harness + `@mirri-ai/acp-adapter` implementation (below) for the migration
 * window.
 *
 * Wire-up (both paths):
 *  - `--login` pivots into the device-code login flow instead of starting
 *    the server — the entry point ACP clients hit via the first-class
 *    `AuthMethodTerminal` path when they re-invoke the agent binary with the
 *    advertised `args:['--login']`.
 *  - `MIRRICODE_HOME` (if set) is forwarded into `authMethods[0].env` so the
 *    `mirri login` subprocess clients spawn for terminal-auth writes its
 *    token under the same data root the ACP server reads from (sandboxed
 *    test setups like Zed's `agent_servers.*.env.MIRRICODE_HOME`).
 *  - `process.argv[1]` is advertised as the legacy `_meta['terminal-auth']`
 *    fallback command.
 */

import type { Command } from 'commander';

import { isLegacyEnabled } from '../experimental-v2';
import { registerNativeAcpCommand } from './acp-native';
import { registerLegacyAcpCommand } from './acp-legacy';

export function registerAcpCommand(parent: Command): void {
  if (!isLegacyEnabled()) {
    registerNativeAcpCommand(parent);
    return;
  }
  registerLegacyAcpCommand(parent);
}

/**
 * The legacy ACP path: an SDK harness + `@mirri-ai/acp-adapter` over the v1
 * engine. Kept only for `MIRRICODE_LEGACY_FLAG=1` during the migration
 * window; removed once the v1 backend retires (see the plan's Phase D).
 */
export { registerLegacyAcpCommand } from './acp-legacy';