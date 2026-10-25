import {
  buildSkillSlashCommands,
  formatSkillCatalogChangeNotice,
  handleSkillsCommand,
  isUserActivatableSkill,
  renderSkillReloadSummary,
} from '#/tui/commands/index';
import type { SkillSummary } from '@mirri-ai/mirri-code-sdk';
import type { SlashCommandHost } from '#/tui/commands';
import { describe, expect, it, vi } from 'vitest';

function skill(
  name: string,
  type?: SkillSummary['type'],
  extra: Partial<SkillSummary> = {},
): SkillSummary {
  return {
    name,
    type,
    description: `${name} skill`,
    ...extra,
  } as SkillSummary;
}

describe('skill slash commands', () => {
  it('allows user-activatable skill types', () => {
    expect(isUserActivatableSkill(skill('default'))).toBe(true);
    expect(isUserActivatableSkill(skill('prompt', 'prompt'))).toBe(true);
    expect(isUserActivatableSkill(skill('inline', 'inline'))).toBe(true);
    expect(isUserActivatableSkill(skill('flow', 'flow'))).toBe(true);
  });

  it('filters non-user-activatable skill types', () => {
    expect(isUserActivatableSkill(skill('agent', 'agent'))).toBe(false);
  });

  it('builds slash commands and command map entries with skill prefixes for non-built-in skills', () => {
    const built = buildSkillSlashCommands([
      skill('review', 'prompt'),
      skill('nested-review', 'prompt', {
        description: 'Nested review skill',
        path: '/skills/parent/nested-review/SKILL.md',
      }),
      skill('agent-only', 'agent'),
      skill('commit', 'flow'),
    ]);

    expect(built.commands.map((command) => command.name)).toEqual([
      'skill:commit',
      'skill:nested-review',
      'skill:review',
    ]);
    expect(built.commands[0]).toMatchObject({
      name: 'skill:commit',
      aliases: [],
      description: 'commit skill',
    });
    expect(built.commands[1]).toMatchObject({
      name: 'skill:nested-review',
      aliases: [],
      description: 'Nested review skill',
    });
    expect([...built.commandMap.entries()]).toEqual([
      ['skill:commit', 'commit'],
      ['skill:nested-review', 'nested-review'],
      ['skill:review', 'review'],
    ]);
  });

  it('sorts built-in skill slash commands before external skill commands', () => {
    const built = buildSkillSlashCommands([
      skill('zeta', 'prompt', { source: 'user' }),
      skill('alpha', 'prompt', { source: 'project' }),
      skill('update-config', 'inline', { source: 'builtin' }),
      skill('mcp-config', 'inline', { source: 'builtin' }),
    ]);

    expect(built.commands.map((command) => command.name)).toEqual([
      'mcp-config',
      'update-config',
      'skill:alpha',
      'skill:zeta',
    ]);
    expect([...built.commandMap.entries()]).toEqual([
      ['mcp-config', 'mcp-config'],
      ['update-config', 'update-config'],
      ['skill:alpha', 'alpha'],
      ['skill:zeta', 'zeta'],
    ]);
  });

  it('keeps disableModelInvocation skills slash-invocable', () => {
    const built = buildSkillSlashCommands([
      skill('mcp-config', 'inline', { disableModelInvocation: true, source: 'builtin' }),
    ]);

    expect(built.commands.map((command) => command.name)).toEqual(['mcp-config']);
    expect(built.commandMap.get('mcp-config')).toBe('mcp-config');
  });

  it('keeps sub-skills slash-invocable', () => {
    const built = buildSkillSlashCommands([
      skill('outer.inner', 'prompt', {
        isSubSkill: true,
        source: 'project',
      }),
    ]);

    expect(built.commands.map((command) => command.name)).toEqual(['outer.inner']);
    expect(built.commandMap.get('outer.inner')).toBe('outer.inner');
  });
});

describe('/skills command', () => {
  it('should show usage when no subcommand is given', async () => {
    const host = makeSkillsHost();

    await handleSkillsCommand(host, '');

    expect(host.showStatus).toHaveBeenCalledWith('Usage: /skills reload');
  });

  it('should show a usage hint when the subcommand is unknown', async () => {
    const host = makeSkillsHost();

    await handleSkillsCommand(host, 'bogus');

    expect(host.showError).toHaveBeenCalledWith(
      'Unknown /skills action: bogus. Usage: /skills reload',
    );
  });

  it('should call reloadSkills and render the per-workspace diff when /skills reload runs', async () => {
    const reloadSkills = vi.fn(async () => ({
      workspaces: [
        { workspace_id: 'ws-1', added: 2, removed: 1, updated: 3, watching: true },
      ],
    }));
    const host = makeSkillsHost({ session: { reloadSkills } });

    await handleSkillsCommand(host, 'reload');

    expect(reloadSkills).toHaveBeenCalledOnce();
    expect(host.showStatus).toHaveBeenCalledWith('Skills reloaded: +2 -1 ~3.');
    expect(host.showError).not.toHaveBeenCalled();
  });

  it('should render the no-change spelling when every diff bucket is empty', async () => {
    const reloadSkills = vi.fn(async () => ({
      workspaces: [
        { workspace_id: 'ws-1', added: 0, removed: 0, updated: 0, watching: true },
      ],
    }));
    const host = makeSkillsHost({ session: { reloadSkills } });

    await handleSkillsCommand(host, 'reload');

    expect(host.showStatus).toHaveBeenCalledWith('Skills reloaded: no changes.');
  });

  it('should append the watcher-off hint when automatic watching is not running', async () => {
    const host = makeSkillsHost();

    renderSkillReloadSummary(host, {
      workspaces: [
        { workspace_id: 'ws-1', added: 1, removed: 0, updated: 0, watching: false },
      ],
    });

    expect(host.showStatus).toHaveBeenCalledWith('Skills reloaded: +1 -0 ~0.');
    expect(host.showStatus).toHaveBeenCalledWith(
      'Automatic skill watching is not running; this reload was manual.',
      'warning',
    );
  });

  it('should render one line per workspace when the summary has multiple workspaces', () => {
    const host = makeSkillsHost();

    renderSkillReloadSummary(host, {
      workspaces: [
        { workspace_id: 'ws-1', added: 1, removed: 0, updated: 0, watching: true },
        { workspace_id: 'ws-2', added: 0, removed: 2, updated: 0, watching: true },
      ],
    });

    expect(host.showStatus).toHaveBeenCalledWith('Skills reloaded (ws-1): +1 -0 ~0.');
    expect(host.showStatus).toHaveBeenCalledWith('Skills reloaded (ws-2): +0 -2 ~0.');
  });

  it('should render the catalog-change notice with the diff summary when a catalogChanged event arrives', () => {
    expect(
      formatSkillCatalogChangeNotice({
        added: ['alpha'],
        removed: [],
        updated: ['beta'],
      }),
    ).toBe('Skills updated (+1 ~1).');
  });

  it('should render the catalog-change notice without a diff summary when all buckets are empty', () => {
    expect(
      formatSkillCatalogChangeNotice({ added: [], removed: [], updated: [] }),
    ).toBe('Skills updated.');
  });
});

function makeSkillsHost({
  session,
}: {
  readonly session?: Record<string, unknown>;
} = {}) {
  return {
    state: { appState: {}, ui: { requestRender: vi.fn() } },
    session,
    requireSession: vi.fn(() => session),
    showStatus: vi.fn(),
    showError: vi.fn(),
  } as unknown as SlashCommandHost & {
    readonly showStatus: ReturnType<typeof vi.fn>;
    readonly showError: ReturnType<typeof vi.fn>;
  };
}
