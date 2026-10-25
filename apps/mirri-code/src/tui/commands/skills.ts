import type { Session, SkillSummary } from '@mirri-ai/mirri-code-sdk';

import { formatErrorMessage } from '../utils/event-payload';
import type { MirriSlashCommand } from './types';
import type { SlashCommandHost } from './dispatch';

export type SkillListSession = Pick<Session, 'listSkills'>;

/** The `/skills reload` session face: listing plus the manual reload RPC. */
export type SkillReloadSession = Pick<Session, 'listSkills' | 'reloadSkills'>;

export interface SkillSlashCommands {
  readonly commands: readonly MirriSlashCommand[];
  readonly commandMap: ReadonlyMap<string, string>;
}

const SKILLS_USAGE = 'Usage: /skills reload';

export function isUserActivatableSkill(skill: SkillSummary): boolean {
  return (
    skill.type === undefined ||
    skill.type === 'prompt' ||
    skill.type === 'inline' ||
    skill.type === 'flow'
  );
}

/**
 * `/skills` — the skill-catalog management face (ADR-010 Task 7): the
 * `reload` subcommand runs the manual `skill.reload` sweep and renders the
 * per-workspace diff, mirroring `/plugins reload`'s status-line result.
 * Any other argument gets the usage hint.
 */
export async function handleSkillsCommand(
  host: SlashCommandHost,
  rawArgs: string,
): Promise<void> {
  const args = rawArgs.trim().split(/\s+/).filter((part) => part.length > 0);
  const sub = args[0];
  const session = host.requireSession();

  try {
    if (sub === undefined) {
      host.showStatus(SKILLS_USAGE);
      return;
    }
    if (sub === 'reload') {
      const summary = await session.reloadSkills();
      renderSkillReloadSummary(host, summary);
      return;
    }
    host.showError(`Unknown /skills action: ${sub}. ${SKILLS_USAGE}`);
  } catch (error) {
    host.showError(`/skills ${sub ?? ''} failed: ${formatErrorMessage(error)}`);
  }
}

/**
 * Format the non-blocking catalog-change notice (US7.2) — the
 * `+N -M ~K` summary the TUI status line renders when a
 * `skill.catalogChanged` event arrives.
 */
export function formatSkillCatalogChangeNotice(
  event: {
    readonly added: readonly string[];
    readonly removed: readonly string[];
    readonly updated: readonly string[];
  },
): string {
  const parts: string[] = [];
  if (event.added.length > 0) parts.push(`+${event.added.length}`);
  if (event.removed.length > 0) parts.push(`-${event.removed.length}`);
  if (event.updated.length > 0) parts.push(`~${event.updated.length}`);
  const summary = parts.length > 0 ? ` (${parts.join(' ')})` : '';
  return `Skills updated${summary}.`;
}

/**
 * Render the `skill.reload` summary (the `reloadPlugins` status-line
 * shape): one line per workspace with its diff counts, the no-change
 * spelling when every diff is empty, and the watcher-off hint when the
 * automatic file watching is not mounted (the reload was the manual result
 * — R4's status face).
 */
export function renderSkillReloadSummary(
  host: SlashCommandHost,
  summary: { readonly workspaces: ReadonlyArray<{
    readonly workspace_id: string;
    readonly added: number;
    readonly removed: number;
    readonly updated: number;
    readonly watching: boolean;
  }> },
): void {
  if (summary.workspaces.length === 0) {
    host.showStatus('Skills reloaded: no changes.');
    return;
  }
  for (const workspace of summary.workspaces) {
    const changed =
      workspace.added > 0 || workspace.removed > 0 || workspace.updated > 0;
    const diff = changed
      ? `+${workspace.added} -${workspace.removed} ~${workspace.updated}`
      : 'no changes';
    const line = summary.workspaces.length > 1
      ? `Skills reloaded (${workspace.workspace_id}): ${diff}.`
      : `Skills reloaded: ${diff}.`;
    host.showStatus(line);
    if (!workspace.watching) {
      host.showStatus(
        'Automatic skill watching is not running; this reload was manual.',
        'warning',
      );
    }
  }
}

function compareSkillSlashCommands(a: SkillSummary, b: SkillSummary): number {
  return (
    getSkillSlashCommandGroup(a.source) - getSkillSlashCommandGroup(b.source) ||
    a.name.localeCompare(b.name)
  );
}

function getSkillSlashCommandGroup(source: SkillSummary['source']): number {
  return source === 'builtin' ? 0 : 1;
}

export function buildSkillSlashCommands(skills: readonly SkillSummary[]): SkillSlashCommands {
  const commandMap = new Map<string, string>();
  const sortedSkills = [...skills].toSorted(compareSkillSlashCommands);
  const commands = sortedSkills.filter(isUserActivatableSkill).map((skill) => {
    const commandName =
      skill.source === 'builtin' || skill.isSubSkill === true
        ? skill.name
        : `skill:${skill.name}`;
    commandMap.set(commandName, skill.name);
    return {
      name: commandName,
      aliases: [],
      description: skill.description ?? '',
    };
  });
  return { commands, commandMap };
}
