import { describe, expect, it } from 'vitest';
import type { AppSkill } from '../src/api/types';
import {
  buildSkillTriggerItems,
  buildSlashItems,
  filterCommands,
  groupSlashItems,
  isBlankSkillDesc,
  isSkillTriggerStart,
  skillDisplayName,
} from '../src/lib/slashCommands';
import { useSlashMenu } from '../src/composables/useSlashMenu';
import type { TextSource } from '../src/composables/useTextSource';

function setup(initialText = '', skills: AppSkill[] = []) {
  // Caret sits at the end of the typed text (realistic editor state); the
  // menu's trigger detection is caret-based (US3.6 word-start rule).
  const field = { text: initialText, caret: initialText.length };
  const inserted: Array<{ display: string; target: string; kind: 'skill' | 'command' }> = [];
  const textSource: TextSource = {
    getText: () => field.text.slice(0, field.caret),
    getCaret: () => field.caret,
    replaceRange(start, end, insert) {
      field.text = field.text.slice(0, start) + insert + field.text.slice(end);
      field.caret = start + insert.length;
    },
    insertSkillMention: (options) => {
      inserted.push(options);
      // The real editor replaces the token and appends a space; simulate the
      // serialized result of that insertion for assertions.
      field.text = `${field.text.slice(0, field.caret)}[$${options.display}] ${field.text.slice(field.caret)}`;
      field.caret = field.text.length;
    },
    focus: () => {},
  };
  const emitted: string[] = [];
  const pushed: string[] = [];
  const slash = useSlashMenu({
    textSource,
    skills: () => skills,
    emitCommand: (cmd) => emitted.push(cmd),
    historyPush: (entry) => pushed.push(entry),
  });
  return { field, inserted, emitted, pushed, slash };
}

describe('useSlashMenu — update', () => {
  it('stays closed for empty text', () => {
    const { slash } = setup('');
    slash.update();
    expect(slash.open.value).toBe(false);
  });

  it('opens and lists commands for a lone slash', () => {
    const { slash } = setup('/');
    slash.update();
    expect(slash.open.value).toBe(true);
    expect(slash.items.value.length).toBeGreaterThan(0);
    expect(slash.active.value).toBe(0);
  });

  it('filters to matching commands', () => {
    const { slash } = setup('/com');
    slash.update();
    expect(slash.open.value).toBe(true);
    expect(slash.items.value.map((i) => i.name)).toContain('/compact');
  });

  it('closes when nothing matches', () => {
    const { slash } = setup('/zzzznotacommand');
    slash.update();
    expect(slash.open.value).toBe(false);
  });

  it('closes once the token contains a space', () => {
    const { slash } = setup('/goal some task');
    slash.update();
    expect(slash.open.value).toBe(false);
  });

  it('closes for text that does not start with a slash', () => {
    const { slash } = setup('hello');
    slash.update();
    expect(slash.open.value).toBe(false);
  });

  it('includes session skills as /<skill-name>', () => {
    const { slash } = setup('/', [{ name: 'deploy', description: 'deploy stuff', source: 'builtin' }]);
    slash.update();
    const names = slash.items.value.map((i) => i.name);
    expect(names).toContain('/deploy');
  });
});

describe('buildSlashItems — disabled skills (B8)', () => {
  it('drops a skill from the slash items when the row is marked disabled', () => {
    const items = buildSlashItems([
      { name: 'pdf', description: 'PDFs', source: 'project', enabled: false },
      { name: 'greet', description: 'Greets', source: 'project' },
    ]);
    const names = items.map((i) => i.name);
    expect(names).not.toContain('/skill:pdf');
    expect(names).toContain('/skill:greet');
  });

  it('keeps a skill when enabled is absent (older backend without the field)', () => {
    const items = buildSlashItems([{ name: 'pdf', description: 'PDFs', source: 'project' }]);
    expect(items.map((i) => i.name)).toContain('/skill:pdf');
  });

  it('keeps a skill when enabled is explicitly true', () => {
    const items = buildSlashItems([
      { name: 'pdf', description: 'PDFs', source: 'project', enabled: true },
    ]);
    expect(items.map((i) => i.name)).toContain('/skill:pdf');
  });

  it('keeps a disabled skill when includeDisabled is set (the composer submit gate)', () => {
    const items = buildSlashItems(
      [{ name: 'pdf', description: 'PDFs', source: 'project', enabled: false }],
      { includeDisabled: true },
    );
    expect(items.map((i) => i.name)).toContain('/skill:pdf');
  });
});

describe('buildSlashItems — /cron command (B6)', () => {
  it('lists /cron among the built-in commands and filters on "cro"', () => {
    const items = buildSlashItems();
    const names = items.map((i) => i.name);
    expect(names).toContain('/cron');
    expect(filterCommands('cro', items).map((i) => i.name)).toEqual(['/cron']);
  });
});

describe('useSlashMenu — select', () => {
  it('non-acceptsInput: clears text, pushes history, emits the command', () => {
    const { field, emitted, pushed, slash } = setup('/new');
    slash.select({ name: '/new', desc: '' });
    expect(field.text).toBe('');
    expect(pushed).toEqual(['/new']);
    expect(emitted).toEqual(['/new']);
    expect(slash.open.value).toBe(false);
  });

  it('acceptsInput: keeps the command in the box and does not emit yet', () => {
    const { field, emitted, pushed, slash } = setup('/goal');
    slash.select({ name: '/goal', desc: '', acceptsInput: true });
    expect(field.text).toBe('/goal ');
    expect(emitted).toEqual([]);
    expect(pushed).toEqual([]);
    expect(slash.open.value).toBe(false);
  });

  it('skill item with acceptsInput on an empty composer inserts an inline link, not the command backfill', () => {
    const skills: AppSkill[] = [
      { name: 'self-review', description: 'Review', source: 'user', path: '/Users/example/skills/self-review/SKILL.md' },
    ];
    const { field, inserted, emitted, pushed, slash } = setup('/self-rev', skills);
    slash.select({ name: '/skill:self-review', desc: '', isSkill: true, acceptsInput: true });
    expect(field.text).toBe('[$self-review] ');
    expect(inserted).toEqual([{ display: 'self-review', target: '/Users/example/skills/self-review/SKILL.md', kind: 'skill' }]);
    expect(emitted).toEqual([]);
    expect(pushed).toEqual([]);
  });

  it('skill item on an empty composer normalizes a directory path to the SKILL.md target', () => {
    const skills: AppSkill[] = [
      { name: 'deploy', description: 'Deploy', source: 'project', path: '/example/deploy' },
    ];
    const { field, inserted, slash } = setup('/dep', skills);
    slash.select({ name: '/skill:deploy', desc: '', isSkill: true, acceptsInput: true });
    expect(field.text).toBe('[$deploy] ');
    expect(inserted).toEqual([{ display: 'deploy', target: '/example/deploy/SKILL.md', kind: 'skill' }]);
  });

  it('Given a builtin skill reported with a builtin:// pseudo path, when it is selected from the slash menu, then the inserted chip keeps the pseudo path verbatim without a /SKILL.md suffix', () => {
    const skills: AppSkill[] = [
      { name: 'write-goal', description: 'Goal help', source: 'builtin', path: 'builtin://write-goal' },
    ];
    const { field, inserted, slash } = setup('/write-goal', skills);
    slash.select({ name: '/skill:write-goal', desc: '', isSkill: true, acceptsInput: true });
    expect(field.text).toBe('[$write-goal] ');
    expect(inserted).toEqual([{ display: 'write-goal', target: 'builtin://write-goal', kind: 'skill' }]);
  });
});

describe('buildSlashItems — /tasks overview command (B7)', () => {
  it('lists /tasks among the built-in commands and filters on "tas"', () => {
    const items = buildSlashItems();
    expect(items.map((i) => i.name)).toContain('/tasks');
    expect(filterCommands('tas', items).map((i) => i.name)).toEqual(['/tasks']);
  });
});

describe('buildSlashItems — builtin skills first (US3.1/US3.4)', () => {
  it('Given a mixed skill list (project skills first, builtin skills last), when buildSlashItems runs, then builtin-sourced skill items precede all other skill items while the non-builtin relative order is preserved', () => {
    const items = buildSlashItems([
      { name: 'greet', description: 'Greets', source: 'project' },
      { name: 'pdf', description: 'PDFs', source: 'project' },
      { name: 'deploy', description: 'Deploy stuff', source: 'builtin' },
    ]);
    const names = items.map((i) => i.name);
    // Then: built-in commands still lead the list untouched, and within the
    // skill group `/deploy` (builtin) comes before the other skills while
    // `/skill:greet` and `/skill:pdf` keep their input order relative to each
    // other.
    expect(names[0]).toBe('/new');
    expect(items.filter((i) => i.isSkill).map((i) => i.name)).toEqual([
      '/deploy',
      '/skill:greet',
      '/skill:pdf',
    ]);
  });
});

describe('isSkillTriggerStart — trigger characters', () => {
  it('should return true when text starts with "$" and contains no space', () => {
    expect(isSkillTriggerStart('$deploy')).toBe(true);
  });

  it('should return true when text starts with "¥" and contains no space', () => {
    expect(isSkillTriggerStart('¥deploy')).toBe(true);
  });

  it('should return true when text starts with "￥" and contains no space', () => {
    expect(isSkillTriggerStart('￥deploy')).toBe(true);
  });

  it('should return false when the trigger text contains a space', () => {
    expect(isSkillTriggerStart('$deploy now')).toBe(false);
  });

  it('should return false when the trigger text contains a full-width space', () => {
    expect(isSkillTriggerStart('$deploy\u3000now')).toBe(false);
  });

  it('should return false when the text is empty', () => {
    expect(isSkillTriggerStart('')).toBe(false);
  });
});

describe('groupSlashItems — commands before skills', () => {
  it('should return commands first then skills while preserving input order when given a mixed list', () => {
    const builtin = buildSlashItems();
    const all = buildSlashItems([
      { name: 'deploy', description: 'Deploy', source: 'builtin' },
      { name: 'greet', description: 'Greets', source: 'project' },
    ]);
    const { commands, skills } = groupSlashItems(all);

    expect(commands.map((i) => i.name)).toEqual(builtin.map((i) => i.name));
    expect(skills.map((i) => i.name)).toEqual(['/deploy', '/skill:greet']);
  });

  it('should keep every item in its input order within each group when items are interleaved', () => {
    const all = buildSlashItems([
      { name: 'a', description: 'A', source: 'project' },
      { name: 'b', description: 'B', source: 'project' },
    ]);
    const interleaved = [all[0], ...all.filter((i) => i.isSkill), all[1]];
    const { commands, skills } = groupSlashItems(interleaved);

    expect(commands.map((i) => i.name)).toEqual(['/new', '/clear']);
    expect(skills.map((i) => i.name)).toEqual(['/skill:a', '/skill:b']);
  });
});

describe('skillDisplayName — bare names', () => {
  it('should strip the leading slash and skill: prefix for skill items', () => {
    expect(skillDisplayName({ name: '/skill:deploy', desc: '', isSkill: true })).toBe('deploy');
    expect(skillDisplayName({ name: '/deploy', desc: '', isSkill: true })).toBe('deploy');
  });

  it('should return the name unchanged for non-skill items', () => {
    expect(skillDisplayName({ name: '/new', desc: '' })).toBe('/new');
  });
});

describe('filterCommands — group order after ranking (ADR-003)', () => {
  it('keeps commands before skills even when a skill ranks higher by score', () => {
    const items = buildSlashItems([
      { name: 'assistant-tools', description: 'Assistant tools', source: 'project' },
    ]);
    // "as" hits the bare skill name at prefix level (score 2) but only matches
    // "/tasks" as a substring (score 1) — ranked output puts the skill first.
    // After regrouping, commands must still come before skills so the menu
    // group order matches the index-based selection semantics.
    const names = filterCommands('as', items).map((i) => i.name);
    expect(names).toEqual(['/tasks', '/skill:assistant-tools']);
  });

  it('preserves score order inside the skills group after regrouping', () => {
    const items = buildSlashItems([
      { name: 'assistant-tools', description: 'Assistant tools', source: 'project' },
      { name: 'cascade', description: 'Cascade', source: 'project' },
    ]);
    const names = filterCommands('as', items).map((i) => i.name);
    expect(names).toEqual(['/tasks', '/skill:assistant-tools', '/skill:cascade']);
  });
});

describe('filterCommands — bare-name matching for skills', () => {
  it('should give /skill:deploy a startsWith-level hit for query "de" and rank it before substring matches', () => {
    const items = buildSlashItems([
      { name: 'cde', description: 'CDE', source: 'project' },
      { name: 'deploy', description: 'Deploy', source: 'project' },
    ]);
    const names = filterCommands('de', items).map((i) => i.name);
    expect(names).toEqual(['/skill:deploy', '/skill:cde']);
  });

  it('should still match /skill:deploy for the full prefix query "skill:"', () => {
    const items = buildSlashItems([{ name: 'deploy', description: 'Deploy', source: 'project' }]);
    const names = filterCommands('skill:', items).map((i) => i.name);
    expect(names).toContain('/skill:deploy');
  });

  it('should still match when the query is the full canonical name "/skill:deploy"', () => {
    const items = buildSlashItems([{ name: 'deploy', description: 'Deploy', source: 'project' }]);
    const names = filterCommands('/skill:deploy', items).map((i) => i.name);
    expect(names).toEqual(['/skill:deploy']);
  });
});

describe('buildSkillTriggerItems — skill-only trigger menu', () => {
  it('should return only skill items with canonical names, raw descriptions and acceptsInput when given any skills', () => {
    const items = buildSkillTriggerItems([
      { name: 'deploy', description: 'Deploy stuff', source: 'builtin' },
      { name: 'greet', description: '部署技能', source: 'project' },
    ]);
    expect(items).toEqual([
      { name: '/deploy', desc: 'Deploy stuff', isSkill: true, acceptsInput: true },
      { name: '/skill:greet', desc: '部署技能', isSkill: true, acceptsInput: true },
    ]);
  });

  it('should drop a disabled skill by default', () => {
    const names = buildSkillTriggerItems([
      { name: 'pdf', description: 'PDFs', source: 'project', enabled: false },
      { name: 'greet', description: 'Greets', source: 'project' },
    ]).map((i) => i.name);
    expect(names).toEqual(['/skill:greet']);
  });

  it('should keep a disabled skill when includeDisabled is true', () => {
    const names = buildSkillTriggerItems(
      [
        { name: 'pdf', description: 'PDFs', source: 'project', enabled: false },
        { name: 'greet', description: 'Greets', source: 'project' },
      ],
      { includeDisabled: true },
    ).map((i) => i.name);
    expect(names).toEqual(['/skill:pdf', '/skill:greet']);
  });
});

describe('isBlankSkillDesc — blank description detection', () => {
  it('should return true for empty, whitespace-only, and symbol-only descriptions', () => {
    expect(isBlankSkillDesc('')).toBe(true);
    expect(isBlankSkillDesc('  ')).toBe(true);
    expect(isBlankSkillDesc('>')).toBe(true);
    expect(isBlankSkillDesc('->')).toBe(true);
  });

  it('should return false when the description contains letters or numbers', () => {
    expect(isBlankSkillDesc('部署技能')).toBe(false);
    expect(isBlankSkillDesc('deploy')).toBe(false);
  });
});
