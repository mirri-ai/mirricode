/**
 * `hookCommandRewrite` domain — registers the `hook-command-rewrite`
 * experimental flag into `flag`.
 *
 * Gates whether PreToolUse shell hooks may return `updatedInput` to modify
 * tool arguments before execution (e.g. rtk integration). Off by default;
 * enable via `MIRRICODE_EXPERIMENTAL_HOOK_COMMAND_REWRITE`, the master
 * `MIRRICODE_EXPERIMENTAL_FLAG`, or the `[experimental]` config section.
 * Security risk: a malicious hook can transparently rewrite commands.
 */

import { type FlagDefinitionInput, registerFlagDefinition } from '#/app/flag/flagRegistry';

export const HOOK_COMMAND_REWRITE_FLAG_ID = 'hook-command-rewrite';
export const HOOK_COMMAND_REWRITE_FLAG_ENV = 'MIRRICODE_EXPERIMENTAL_HOOK_COMMAND_REWRITE';

export const hookCommandRewriteFlag: FlagDefinitionInput = {
  id: HOOK_COMMAND_REWRITE_FLAG_ID,
  title: 'Hook command rewriting',
  description:
    'Allow PreToolUse shell hooks to return updatedInput to modify tool ' +
    'arguments before execution (e.g. rtk integration). Security risk: a ' +
    'malicious hook can transparently rewrite commands.',
  env: HOOK_COMMAND_REWRITE_FLAG_ENV,
  default: false,
  surface: 'core',
};

registerFlagDefinition(hookCommandRewriteFlag);
