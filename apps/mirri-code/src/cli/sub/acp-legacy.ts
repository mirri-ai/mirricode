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

import { createMirriCodeHostIdentity, getVersion } from '#/cli/version';
import { buildSkillSlashCommands } from '#/tui/commands/skills';

export function registerLegacyAcpCommand(parent: Command): void {
  parent
    .command('acp')
    .description('Run mirri-code as an Agent Client Protocol (ACP) server over stdio.')
    .action(async () => {
      const identity = createMirriCodeHostIdentity();
      const harness = createMirriHarness({
        identity,
        uiMode: 'acp',
      });
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
          agentInfo: { name: 'Mirri Code', version: getVersion() },
          slashCommands: resolveSlashCommands,
        });
        process.exit(0);
      } catch (error) {
        process.stderr.write(`acp server: fatal error: ${String(error)}\n`);
        process.exit(1);
      }
    });
}