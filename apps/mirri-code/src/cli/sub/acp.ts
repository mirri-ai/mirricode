/**
 * `mirri acp` sub-command routing.
 *
 * By default the command delegates to the agent-core-v2 ACP server
 * (`@mirri-ai/acp-server`), loaded lazily so parsing the CLI does not
 * initialize the engine. A truthy `MIRRICODE_LEGACY_FLAG` keeps the SDK
 * harness + `@mirri-ai/acp-adapter` implementation (below) for the migration
 * window.
 *
 * Managed login is retired (SDR-004): neither path offers a `--login` flag
 * or advertises terminal-auth.
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