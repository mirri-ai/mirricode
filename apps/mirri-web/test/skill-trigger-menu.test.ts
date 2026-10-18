import { describe, expect, it, vi } from 'vitest';
import type { AppSkill } from '../src/api/types';
import type { SlashCommand } from '../src/lib/slashCommands';
import { skillReferenceTarget } from '../src/lib/skillReferenceTarget';
import { useSkillTriggerMenu } from '../src/composables/useSkillTriggerMenu';
import type { TextSource } from '../src/composables/useTextSource';

interface Inserted {
  display: string;
  target: string;
  kind: 'skill' | 'command';
}

function setup(initialText = '', skills: AppSkill[] = []) {
  const field = { text: initialText, caret: initialText.length };
  const focus = vi.fn();
  const inserted: Inserted[] = [];
  const textSource: TextSource = {
    getText: () => field.text.slice(0, field.caret),
    getCaret: () => field.caret,
    replaceRange(start, end, insert) {
      field.text = field.text.slice(0, start) + insert + field.text.slice(end);
      field.caret = start + insert.length;
    },
    insertSkillMention: (options) => {
      inserted.push(options);
    },
    focus,
  };
  const menu = useSkillTriggerMenu({
    textSource,
    skills: () => skills,
  });
  return { field, inserted, focus, menu };
}

const SKILLS: AppSkill[] = [
  { name: 'deploy', description: 'Deploy the app', source: 'builtin' },
  { name: 'greet', description: 'Greets the user', source: 'project' },
];

describe('useSkillTriggerMenu — update', () => {
  it('should open with only skill items when text starts with "$"', () => {
    const { menu } = setup('$', SKILLS);
    menu.update();
    expect(menu.open.value).toBe(true);
    expect(menu.items.value.length).toBeGreaterThan(0);
    expect(menu.items.value.every((i) => i.isSkill === true)).toBe(true);
    expect(menu.active.value).toBe(0);
  });

  it('should open with only skill items when text starts with "¥"', () => {
    const { menu } = setup('¥', SKILLS);
    menu.update();
    expect(menu.open.value).toBe(true);
    expect(menu.items.value.every((i) => i.isSkill === true)).toBe(true);
  });

  it('should open with only skill items when text starts with "￥"', () => {
    const { menu } = setup('￥', SKILLS);
    menu.update();
    expect(menu.open.value).toBe(true);
    expect(menu.items.value.every((i) => i.isSkill === true)).toBe(true);
  });

  it('should stay closed when the token contains a space', () => {
    const { menu } = setup('$deploy now', SKILLS);
    menu.update();
    expect(menu.open.value).toBe(false);
  });

  it('should stay closed when the text does not start with a trigger', () => {
    const { menu } = setup('hello', SKILLS);
    menu.update();
    expect(menu.open.value).toBe(false);
  });

  it('should narrow the candidates to bare-name matches when the user keeps typing', () => {
    const { menu } = setup('$gre', SKILLS);
    menu.update();
    expect(menu.open.value).toBe(true);
    expect(menu.items.value.map((i) => i.name)).toEqual(['/skill:greet']);
  });

  it('should close when no skill matches the query', () => {
    const { menu } = setup('$zznomatch', SKILLS);
    menu.update();
    expect(menu.open.value).toBe(false);
  });

  it('should exclude disabled skills from the candidates when the user-level flag is false', () => {
    const { menu } = setup('$', [
      { name: 'pdf', description: 'PDFs', source: 'project', enabled: false },
      { name: 'greet', description: 'Greets', source: 'project' },
    ]);
    menu.update();
    const names = menu.items.value.map((i) => i.name);
    expect(names).not.toContain('/skill:pdf');
    expect(names).toContain('/skill:greet');
  });
});

describe('useSkillTriggerMenu — select', () => {
  it('should insert a skillMention chip with a trailing space via the editor for a project skill (unified chip form)', () => {
    const { inserted, menu } = setup('$gre', SKILLS);
    menu.select({ name: '/skill:greet', desc: 'Greets', isSkill: true, acceptsInput: true } satisfies SlashCommand);
    expect(inserted).toEqual([{ display: 'greet', target: 'command:/skill:greet', kind: 'skill' }]);
    expect(menu.open.value).toBe(false);
  });

  it('should insert a skillMention chip for a builtin-sourced skill', () => {
    const { inserted, menu } = setup('$de', SKILLS);
    menu.select({ name: '/deploy', desc: 'Deploy', isSkill: true, acceptsInput: true } satisfies SlashCommand);
    expect(inserted).toEqual([{ display: 'deploy', target: 'command:/deploy', kind: 'skill' }]);
  });

  it('should insert a path-resolved target when the skill reports a SKILL.md path', () => {
    const withPath = [{ name: 'greet', description: 'G', source: 'project', path: '/skills/greet/SKILL.md' }];
    const { inserted, menu } = setup('$gre', withPath);
    menu.select({ name: '/skill:greet', desc: 'G', isSkill: true, acceptsInput: true } satisfies SlashCommand);
    expect(inserted).toEqual([{ display: 'greet', target: '/skills/greet/SKILL.md', kind: 'skill' }]);
  });

  it('should insert the builtin pseudo path verbatim without a SKILL.md suffix when the skill is builtin-sourced', () => {
    const builtins = [
      { name: 'update-config', description: 'Config', source: 'builtin' as const, path: 'builtin://update-config' },
    ];
    const { inserted, menu } = setup('$upd', builtins);
    menu.select({ name: '/update-config', desc: 'Config', isSkill: true, acceptsInput: true } satisfies SlashCommand);
    expect(inserted).toEqual([
      { display: 'update-config', target: 'builtin://update-config', kind: 'skill' },
    ]);
  });

  it('should backfill the canonical command name for a bare non-skill (mixin) item', () => {
    const { field, inserted, focus, menu } = setup('$de', SKILLS);
    menu.select({ name: '/deploy', desc: 'Deploy', isSkill: false, acceptsInput: true } satisfies SlashCommand);
    expect(inserted).toEqual([]); // no chip for the command fallback
    expect(field.text).toBe('/deploy ');
    expect(focus).toHaveBeenCalledTimes(1);
  });
});

describe('skillReferenceTarget — target resolution', () => {
  it('should return the command fallback when the skill reports no path', () => {
    expect(skillReferenceTarget(undefined, '/skill:greet')).toBe('command:/skill:greet');
    expect(skillReferenceTarget({ path: undefined }, '/skill:greet')).toBe('command:/skill:greet');
  });

  it('should return a SKILL.md path verbatim', () => {
    expect(skillReferenceTarget({ path: '/skills/greet/SKILL.md' }, '/skill:greet')).toBe('/skills/greet/SKILL.md');
  });

  it('should append /SKILL.md to a bare directory path from an older engine', () => {
    expect(skillReferenceTarget({ path: '/skills/greet/' }, '/skill:greet')).toBe('/skills/greet/SKILL.md');
    expect(skillReferenceTarget({ path: '/skills/greet' }, '/skill:greet')).toBe('/skills/greet/SKILL.md');
  });

  it('should return the builtin pseudo path verbatim without appending /SKILL.md', () => {
    expect(skillReferenceTarget({ path: 'builtin://update-config' }, '/update-config')).toBe('builtin://update-config');
    expect(skillReferenceTarget({ path: 'builtin://sub-skill/review' }, '/skill:sub-skill.review')).toBe('builtin://sub-skill/review');
  });
});
