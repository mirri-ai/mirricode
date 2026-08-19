/**
 * `mirri server` — deprecated (v1 server retirement).
 *
 * The v1 server package (`packages/server`, `@mirri-ai/server` — REST +
 * WebSocket engine) has been removed from the repository. `mirri server` is
 * now a deprecation shim: any invocation — bare or with legacy subcommands or
 * flags — prints a deprecation notice to stderr and exits 1. The shim is
 * scheduled for removal in the next major version of Mirri Code.
 *
 * One subcommand stays functional: `mirri server kill`, the cleanup path for
 * background daemons started by pre-0.28.0 builds (implemented in
 * `web/legacy-kill.ts`).
 *
 * The OS service-manager subcommands (`install/uninstall/start/stop/restart/
 * status`, previously `./lifecycle.ts` + `packages/server/src/svc/*`) are not
 * carried over: the product now runs the server via `mirri web`, which stays
 * attached to the terminal (Ctrl+C stops it), so service-ization is gone
 * entirely (matching the kimi-code retirement).
 *
 * The top-level `mirri web` command (v2 engine / kap-server) is registered
 * here via `registerWebCommand`, so the CLI entry point (`cli/commands.ts`)
 * keeps calling `registerServerCommand` unchanged.
 */

import type { Command } from 'commander';

import { registerDeprecatedServerCommand } from '../web/deprecated-server';
import { registerWebCommand } from '../web';

export function registerServerCommand(program: Command): void {
  // Deprecated `mirri server` tree: deprecation notice + exit 1, preserving
  // only `server kill` (legacy pre-0.28.0 daemon cleanup).
  registerDeprecatedServerCommand(program);

  // `mirri web` — the v2 server engine (kap-server), the only live server
  // command. Registered here so the entry point shape is untouched.
  registerWebCommand(program);
}