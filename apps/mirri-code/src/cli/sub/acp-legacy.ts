/**
 * Legacy `mirri acp` implementation — the SDK-harness path over
 * `@mirri-ai/acp-adapter` (v1 engine). Kept for `MIRRICODE_LEGACY_FLAG=1`
 * during the migration window; removed together with the v1 backend.
 */

import type { Command } from 'commander';

import {
  ACP_BUILTIN_SLASH_COMMANDS,
  runAcpServer,
  type AvailableCommand,
  type SlashCommandsSnapshot,
} from '@mirri-ai/acp-adapter';
import { createMirriHarness, type Session, type SkillSummary } from '@mirri-ai/mirri-code-sdk';

import { MIRRICODE_HOME_ENV } from '#/constant/app';
import { createMirriCodeHostIdentity, getVersion } from '#/cli/version';
import { buildSkillSlashCommands } from '#/tui/commands/skills';

import { runLoginFlow } from './login-flow';

export function registerLegacyAcpCommand(parent: Command): void {
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
      const identity = createMirriCodeHostIdentity();
      const harness = createMirriHarness({
        identity,
        uiMode: 'acp',
      });
      // Forward `MIRRICODE_HOME` (if set) into `authMethods[0].env` so the
      // `mirri login` subprocess clients spawn for terminal-auth writes its
      // token under the same data root the ACP server reads from.
      const sandboxHome = process.env[MIRRICODE_HOME_ENV];
      const terminalAuthEnv =
        sandboxHome !== undefined && sandboxHome.length > 0
          ? { [MIRRICODE_HOME_ENV]: sandboxHome }
          : undefined;
      // Legacy `_meta.terminal-auth` fallback for clients that don't yet
      // honor the first-class `type:'terminal'`.
      const legacyCommand = process.argv[1];
      const builtinCommands: AvailableCommand[] = (
        ACP_BUILTIN_SLASH_COMMANDS as readonly AvailableCommand[]
      ).map((cmd) => ({
        name: cmd.name,
        description: cmd.description,
        input: cmd.input,
      }));
      // Skills are session-scoped (per-cwd config), so we defer the
      // listSkills() call until the adapter hands us the just-created
      // Session. A listSkills() failure degrades to builtins-only so a
      // broken skill source never blanks the palette.
      const resolveSlashCommands = async (
        session: Session,
      ): Promise<SlashCommandsSnapshot> => {
        let skills: readonly SkillSummary[] = [];
        try {
          skills = await session.listSkills();
        } catch {
          skills = [];
        }
        const built = buildSkillSlashCommands(skills);
        const skillCommands = built.commands.map((cmd) => ({
          name: cmd.name,
          description: cmd.description,
        }));
        return {
          commands: [...builtinCommands, ...skillCommands],
          skillCommandMap: built.commandMap,
        };
      };
      try {
        await runAcpServer(harness, {
          agentInfo: { name: 'Mirri Code CLI', version: getVersion() },
          slashCommands: resolveSlashCommands,
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