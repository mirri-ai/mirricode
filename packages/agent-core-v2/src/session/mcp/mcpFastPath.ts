/**
 * `mcp` domain — registers the `mcp.disableFirstStepFastPath` experimental
 * flag into `flag`.
 *
 * Rollback switch for the first-step MCP readiness fast path: when enabled,
 * session and agent creation once again awaits the workspace MCP initial
 * load (the pre-fast-path behavior). Off by default — the fast path (MCP
 * readiness awaited at the first LLM step instead of at creation) is the
 * default. Enable via `MIRRICODE_DISABLE_MCP_FAST_PATH`, the master
 * `MIRRICODE_EXPERIMENTAL_FLAG`, or the `[experimental]` config section.
 */

import { type FlagDefinitionInput, registerFlagDefinition } from '#/app/flag/flagRegistry';

export const MCP_FAST_PATH_DISABLE_FLAG = 'mcp.disableFirstStepFastPath';
export const MCP_FAST_PATH_DISABLE_FLAG_ENV = 'MIRRICODE_DISABLE_MCP_FAST_PATH';

export const mcpFastPathFlag: FlagDefinitionInput = {
  id: MCP_FAST_PATH_DISABLE_FLAG,
  title: 'Disable MCP first-step fast path',
  description:
    'Restore awaiting the workspace MCP initial load during session and ' +
    'agent creation, instead of deferring it to the first LLM step. ' +
    'Rollback switch only; off by default.',
  env: MCP_FAST_PATH_DISABLE_FLAG_ENV,
  default: false,
  surface: 'core',
};

registerFlagDefinition(mcpFastPathFlag);