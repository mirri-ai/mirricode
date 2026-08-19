/**
 * Native `mirri acp` implementation.
 *
 * Starts the Agent Client Protocol (ACP) server backed directly by the
 * DI × Scope agent engine (`agent-core-v2`) over stdio, so ACP-compatible
 * clients can drive a mirri-code session on the default engine.
 *
 * `@mirri-ai/acp-server` (and its agent-core-v2 engine) is loaded via a
 * lazy dynamic import so parsing the CLI does not initialize the ACP engine.
 */
import type { Command } from 'commander';

import { MIRRICODE_HOME_ENV } from '#/constant/app';
import { getVersion } from '#/cli/version';
import { getDataDir } from '#/utils/paths';

import { runLoginFlow } from './login-flow';

export function registerNativeAcpCommand(parent: Command): void {
  parent
    .command('acp')
    .description('Run mirri-code as an Agent Client Protocol (ACP) server over stdio.')
    .option(
      '--login',
      'Run the device-code login flow then exit (entry point for ACP terminal-auth).',
      false,
    )
    .action(async (opts: { login?: boolean }) => {
      if (opts.login === true) {
        await runLoginFlow();
        return;
      }
      // Forward `MIRRICODE_HOME` (if set) into `authMethods[0].env` so the
      // login subprocess clients spawn for terminal-auth writes its token
      // under the same data root the ACP server reads from (sandboxed test
      // setups like Zed's `agent_servers.*.env.MIRRICODE_HOME`).
      const sandboxHome = process.env[MIRRICODE_HOME_ENV];
      const terminalAuthEnv =
        sandboxHome !== undefined && sandboxHome.length > 0
          ? { [MIRRICODE_HOME_ENV]: sandboxHome }
          : undefined;
      // Legacy `_meta.terminal-auth` fallback for clients that don't yet
      // honor the first-class `type:'terminal'`. `command` is the absolute
      // path to this very binary so the client can spawn it for login.
      const legacyCommand = process.argv[1];
      try {
        const { runAcpServer } = await import('@mirri-ai/acp-server');
        await runAcpServer({
          homeDir: getDataDir(),
          agentInfo: { name: 'Mirri Code CLI', version: getVersion() },
          ...(terminalAuthEnv ? { terminalAuthEnv } : {}),
          ...(legacyCommand !== undefined && legacyCommand.length > 0
            ? { terminalAuthLegacyCommand: legacyCommand }
            : {}),
        });
        process.exit(0);
      } catch (error) {
        process.stderr.write(`acp server: fatal error: ${String(error)}\n`);
        process.exit(1);
      }
    });
}