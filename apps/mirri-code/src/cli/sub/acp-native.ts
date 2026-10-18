/**
 * Native `mirri acp` implementation.
 *
 * Starts the Agent Client Protocol (ACP) server backed directly by the
 * DI × Scope agent engine (`agent-core-v2`) over stdio, so ACP-compatible
 * clients can drive a mirri-code session on the default engine.
 *
 * A truthy `MIRRICODE_RUST_ENGINE` replaces the in-process TS ACP server
 * with the Rust `mirri-acp` binary, spawned as a child process with stdio
 * passthrough (see `./acp-rust.ts`).
 *
 * `@mirri-ai/acp-server` (and its agent-core-v2 engine) is loaded via a
 * lazy dynamic import so parsing the CLI does not initialize the ACP engine.
 */
import type { Command } from 'commander';

import { getVersion } from '#/cli/version';
import { getDataDir } from '#/utils/paths';
import { isRustEngineEnabled } from '#/cli/experimental-v2';

import { runRustAcpServer } from './acp-rust';

export function registerNativeAcpCommand(parent: Command): void {
  parent
    .command('acp')
    .description('Run mirri-code as an Agent Client Protocol (ACP) server over stdio.')
    .action(async () => {
      // Rust engine: spawn the `mirri-acp` binary with stdio passthrough.
      if (isRustEngineEnabled()) {
        try {
          await runRustAcpServer();
        } catch (error) {
          process.stderr.write(`acp server (rust): fatal error: ${String(error)}\n`);
          process.exit(1);
        }
        return;
      }

      try {
        const { runAcpServer } = await import('@mirri-ai/acp-server');
        // Managed login is retired: the ACP server advertises no auth
        // methods, so nothing is forwarded (no terminalAuthEnv / legacy
        // command options exist anymore).
        await runAcpServer({
          homeDir: getDataDir(),
          agentInfo: { name: 'Mirri Code CLI', version: getVersion() },
        });
        process.exit(0);
      } catch (error) {
        process.stderr.write(`acp server: fatal error: ${String(error)}\n`);
        process.exit(1);
      }
    });
}