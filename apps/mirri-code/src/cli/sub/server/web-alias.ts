/**
 * `mirri web` — open the Mirri web UI.
 *
 * Shares the exact same code path as `mirri server run`: it is registered via
 * the same `buildRunCommand` builder (and therefore the same `handleRunCommand`
 * handler and the same ready banner) with two flipped defaults — `defaultOpen`
 * opens the browser, and `defaultForeground` runs the server in the foreground
 * (this terminal stays attached until Ctrl+C) instead of backgrounding a
 * daemon. `--background` opts back into the daemon behavior of `server run`.
 */

import type { Command } from 'commander';

import { buildRunCommand } from './run';

export function registerWebAliasCommand(program: Command): void {
  buildRunCommand(
    program
      .command('web')
      .description('Open the Mirri web UI (runs the server in the foreground by default).'),
    { defaultOpen: true, defaultForeground: true },
  );
}
