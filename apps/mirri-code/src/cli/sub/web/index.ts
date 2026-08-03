/**
 * `mirri web` — run the local Mirri server (REST + WebSocket + web UI) in the
 * foreground and open the web UI in the default browser.
 *
 * Uses `@mirri-ai/kap-server` (the v2 engine). The old `mirri server run`
 * command stays on v1 for backward compatibility.
 *
 * The command itself is the runner (`mirri web` = start the server + open the
 * browser; `--no-open` to skip). The server stays attached to the terminal
 * and stops with Ctrl+C.
 */

import type { Command } from 'commander';

import { registerRotateTokenCommand } from './rotate-token';
import { buildWebCommand } from './run';

export function registerWebCommand(program: Command): void {
  const web = buildWebCommand(
    program
      .command('web')
      .description('Run the local Mirri server (v2 engine) and open the web UI.'),
  );
  registerRotateTokenCommand(web);
}
