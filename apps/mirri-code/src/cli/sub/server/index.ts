/**
 * `mirri server` parent command. Mounts:
 *   - `server run` (background daemon by default; `--foreground` to attach; the
 *     detached daemon child runs the same command with `--daemon`)
 *
 * The OS service-manager subcommands (`install/uninstall/start/stop/restart/
 * status`) are temporarily NOT registered — see the commented
 * `addLifecycleCommands(server)` below. Their implementation is preserved in
 * `./lifecycle.ts` + `packages/server/src/svc/*` for later re-exposure.
 *
 * The top-level `mirri web` command (v2 engine / kap-server) is registered
 * separately via `registerWebCommand` so it stays at the program root.
 */

import type { Command } from 'commander';

import { registerWebCommand } from '../web';
import { registerPsCommand } from './ps';
import { registerKillCommand } from './kill';
import { buildRunCommand } from './run';
import { registerRotateTokenCommand } from './rotate-token';

export function registerServerCommand(program: Command): void {
  const server = program
    .command('server')
    .description('Run the local Mirri API server (REST + WebSocket; the web UI is served by `mirri web`).');

  buildRunCommand(
    server.command('run').description('Start the Mirri server (background daemon; use --foreground to attach).'),
    {},
  );

  registerPsCommand(server);

  registerKillCommand(server);

  registerRotateTokenCommand(server);

  // OS service-manager commands (`install/uninstall/start/stop/restart/status`)
  // are temporarily hidden — the product now favors the on-demand background
  // daemon (`mirri web`) over service-ization. The implementation still lives in
  // `./lifecycle.ts` + `packages/server/src/svc/*`; re-import
  // `addLifecycleCommands` and call it here to re-expose.
  // addLifecycleCommands(server);

  // `mirri web` uses kap-server (v2 engine); the old `mirri server run` stays
  // on the v1 server.
  registerWebCommand(program);
}
