// apps/mirri-web/src/lib/slashCommands.test.ts
// Pure-function tests for the slash/skill menu filtering pipeline —
// the match ranking contract `useSlashMenu` / `useSkillTriggerMenu`
// render on (ADR-003's command-before-skill grouping is asserted at the
// filter boundary, since the menu renders the returned order verbatim).
import { describe, expect, it } from 'vitest';

import {
  SKILL_COMMAND_PREFIX,
  buildSlashItems,
  filterCommands,
  skillDisplayName,
  skillSourceBadgeText,
  skillSourceBadgeVariant,
  type SlashCommand,
} from './slashCommands';

function skillItems(names: string[]): SlashCommand[] {
  return buildSlashItems(
    names.map((name) => ({ name, description: `${name} description`, source: 'user' })),
  ).filter((item) => item.isSkill);
}

describe('filterCommands — namespace-hit demotion', () => {
  it('should rank a skill whose bare name contains the query above every namespace-only hit when the query matches the skill: prefix', async () => {
    const { filterCommands: fc } = await import('./slashCommands');
    // Given the namespace-only hits come FIRST in the input order — if the
    // ranking were flat (same score), they would stay ahead of the
    // name-bearing skill and the bug this demotion fixes would reproduce.
    const items = skillItems(['write-tui', 'deploy', 'write-a-skill']);

    // When the query "skill" is typed ($skill / /skill)
    const listed = fc('skill', items);

    // Then the skill whose NAME carries "skill" comes first (substring of
    // the bare name), and the namespace-only hits (write-tui, deploy —
    // matched only through their `skill:` command prefix) rank below it.
    const names = listed.map((item) => item.name);
    expect(names.indexOf(`/skill:write-a-skill`)).toBeLessThan(names.indexOf(`/skill:write-tui`));
    expect(names.indexOf(`/skill:write-a-skill`)).toBeLessThan(names.indexOf(`/skill:deploy`));
  });

  it('should keep namespace-only hits visible but last when the query is a prefix of skill:', async () => {
    const { filterCommands: fc } = await import('./slashCommands');
    const items = skillItems(['write-tui', 'deploy']);

    // When the query "sk" is typed
    const listed = fc('sk', items);

    // Then every non-builtin skill still surfaces (the namespace hit is
    // demoted, not dropped) — typing the namespace still browses all skills.
    expect(listed.map((item) => item.name)).toEqual(
      expect.arrayContaining([`/skill:write-tui`, `/skill:deploy`]),
    );
  });

  it('should not demote a query that includes the colon separator', async () => {
    const { filterCommands: fc } = await import('./slashCommands');
    const items = skillItems(['write-tui', 'deploy']);

    // When the query spells past the namespace ("skill:de") — an explicit
    // qualified lookup
    const listed = fc('skill:de', items);

    // Then it matches at prefix level on the canonical name (no namespace
    // demotion: the query is not confined to the skill: segment).
    expect(listed.map((item) => item.name)).toEqual([`/skill:deploy`]);
  });

  it('should keep the bare-name match tier above the namespace tier for non-colon queries', async () => {
    const { filterCommands: fc } = await import('./slashCommands');
    const items = skillItems(['slack-notify', 'write-tui']);

    // When the query "sl" is typed — a bare-name prefix hit competes with a
    // namespace prefix hit
    const listed = fc('sl', items);

    // Then the bare-name prefix match (slack-notify) outranks the
    // namespace-only match (write-tui via skill:).
    expect(listed[0]!.name).toBe(`/skill:slack-notify`);
  });

  it('should keep builtin skills at their regular tier when their bare name matches a namespace-looking query', async () => {
    const { buildSlashItems, filterCommands: fc } = await import('./slashCommands');
    // Given a builtin-sourced skill whose command name is bare (`/<name>`)
    // and starts like the namespace qualifier ("sk...")
    const items = buildSlashItems([
      { name: 'sky-plot', description: 'sky', source: 'builtin' },
      { name: 'write-tui', description: 'tui', source: 'user' },
    ]).filter((item) => item.isSkill);

    // When the query "sk" is typed
    const listed = fc('sk', items);

    // Then the builtin's bare-name prefix match keeps its regular tier —
    // the demotion exists only for the `skill:` qualifier segment, which a
    // bare command name does not have.
    expect(listed[0]!.name).toBe(`/sky-plot`);
  });
});

describe('filterCommands — baseline ranking (regression guard)', () => {
  it('should rank exact above prefix above substring on bare names', async () => {
    const { filterCommands: fc } = await import('./slashCommands');
    const items = skillItems(['de', 'deploy', 'code-deploy-x']);

    const listed = fc('de', items);

    // exact (de) → prefix (deploy) → substring (code-deploy-x)
    expect(listed.map((item) => item.name)).toEqual([
      `/skill:de`,
      `/skill:deploy`,
      `/skill:code-deploy-x`,
    ]);
  });

  it('should return every item unfiltered when the query is empty', async () => {
    const { filterCommands: fc } = await import('./slashCommands');
    const items = skillItems(['alpha', 'beta']);

    expect(fc('', items).length).toBe(items.length);
  });
});

describe('SKILL_COMMAND_PREFIX', () => {
  it('should stay the canonical namespace qualifier', () => {
    expect(SKILL_COMMAND_PREFIX).toBe('skill:');
  });
});

describe('buildSlashItems — source passthrough', () => {
  it('should carry source and plugin onto skill items when building slash items', () => {
    const items = buildSlashItems([
      { name: 'deploy', description: 'Deploy', source: 'builtin' },
      { name: 'greeter', description: 'Greets', source: 'extra', plugin: 'demo-plugin' },
      { name: 'my-tool', description: 'Mine', source: 'project' },
    ]);
    const deploy = items.find((i) => i.name === '/deploy');
    const greeter = items.find((i) => i.name === '/skill:greeter');
    const mine = items.find((i) => i.name === '/skill:my-tool');
    expect(deploy?.source).toBe('builtin');
    expect(deploy?.plugin).toBeUndefined();
    expect(greeter?.source).toBe('extra');
    expect(greeter?.plugin).toBe('demo-plugin');
    expect(mine?.source).toBe('project');
  });

  it('should omit source and plugin on built-in command items', () => {
    const items = buildSlashItems([{ name: 'deploy', description: 'Deploy', source: 'builtin' }]);
    const command = items.find((i) => i.name === '/new');
    expect(command?.source).toBeUndefined();
    expect(command?.plugin).toBeUndefined();
  });
});

describe('filterCommands — source and plugin match keys', () => {
  const skills = [
    { name: 'deploy', description: 'Deploy', source: 'builtin' },
    { name: 'lint-it', description: 'Lint', source: 'project' },
    { name: 'greeter', description: 'Greets', source: 'extra', plugin: 'demo-plugin' },
  ];

  it('should keep only project skills when the query is the canonical source token', () => {
    const items = filterCommands('project', buildSlashItems(skills));
    const names = items.filter((i) => i.isSkill).map((i) => skillDisplayName(i));
    expect(names).toEqual(['lint-it']);
  });

  it('should keep only a plugin skills when the query is the plugin id', () => {
    const items = filterCommands('demo-plugin', buildSlashItems(skills));
    const names = items.filter((i) => i.isSkill).map((i) => skillDisplayName(i));
    expect(names).toEqual(['greeter']);
  });

  it('should match a plugin id on a substring like skill names do', () => {
    const items = filterCommands('demo', buildSlashItems(skills));
    const names = items.filter((i) => i.isSkill).map((i) => skillDisplayName(i));
    expect(names).toEqual(['greeter']);
  });

  it('should not match localized source labels like 额外 when filtering', () => {
    const items = filterCommands('额外', buildSlashItems(skills));
    expect(items.filter((i) => i.isSkill)).toEqual([]);
  });

  it('should still clear the list when the query matches nothing', () => {
    const items = filterCommands('nomatch-zzz', buildSlashItems(skills));
    expect(items).toEqual([]);
  });
});

describe('skillSourceBadgeText', () => {
  const T = (key: string): string => key; // identity — assert the KEY, not the label

  const skillItem = (name: string, source?: string, plugin?: string): SlashCommand => ({
    name: source === 'builtin' ? `/${name}` : `/skill:${name}`,
    desc: 'A skill',
    isSkill: true,
    acceptsInput: true,
    source,
    plugin,
  });

  it('should return the plugin id as the badge text when the skill carries a plugin', () => {
    expect(skillSourceBadgeText(skillItem('greeter', 'extra', 'demo-plugin'), T)).toBe('demo-plugin');
  });

  it('should return the localized key for each known source when no plugin id is present', () => {
    expect(skillSourceBadgeText(skillItem('deploy', 'builtin'), T)).toBe('settings.skillsSourceBuiltin');
    expect(skillSourceBadgeText(skillItem('lint-it', 'project'), T)).toBe('settings.skillsSourceProject');
    expect(skillSourceBadgeText(skillItem('mine', 'user'), T)).toBe('settings.skillsSourceUser');
    expect(skillSourceBadgeText(skillItem('thing', 'extra'), T)).toBe('settings.skillsSourceExtra');
  });

  it('should degrade to the extra label when a plugin skill has no plugin id', () => {
    expect(skillSourceBadgeText(skillItem('thing', 'extra'), T)).toBe('settings.skillsSourceExtra');
  });

  it('should render an unknown source verbatim instead of swallowing it', () => {
    expect(skillSourceBadgeText(skillItem('odd', 'future-scope'), T)).toBe('future-scope');
  });

  it('should return undefined for commands and skills with no source', () => {
    expect(skillSourceBadgeText({ name: '/new', desc: 'commands.new.desc' }, T)).toBeUndefined();
    expect(skillSourceBadgeText(skillItem('bare'), T)).toBeUndefined();
  });
});

describe('skillSourceBadgeVariant', () => {
  it('should map each known source to its SkillsPanel badge variant', () => {
    expect(skillSourceBadgeVariant('project', undefined)).toBe('info');
    expect(skillSourceBadgeVariant('user', undefined)).toBe('neutral');
    expect(skillSourceBadgeVariant('extra', undefined)).toBe('success');
    expect(skillSourceBadgeVariant('builtin', undefined)).toBe('solid');
  });

  it('should use the extra variant for plugin skills whether or not the plugin id is present', () => {
    expect(skillSourceBadgeVariant('extra', 'demo-plugin')).toBe('success');
  });

  it('should fall back to neutral for unknown sources', () => {
    expect(skillSourceBadgeVariant('future-scope', undefined)).toBe('neutral');
  });
});
