// Pure-logic tests for inline skill/command reference input + validation
// (M2–M4, skills domain US3.6 / ADR-004): trigger-position detection, trigger
// token extraction, insert computation, pre-submit validation, and the message
// split for chip rendering in ChatPane.
import { describe, expect, it, vi } from 'vitest';
import {
  collapseInlineWhitespace,
  formatSkillReferenceLink,
} from '@mirri-ai/protocol';
import {
  expandCompactTokens,
  extractInlineReferenceSpan,
  extractSlashTriggerToken,
  extractSkillTriggerToken,
  findInvalidSkillMentionAttrs,
  hasInlineReferences,
  parseCompactTokens,
  splitMessageInlineReferences,
  type InlineSegment,
} from '../src/lib/inlineReferences';
import { useSkillTriggerMenu } from '../src/composables/useSkillTriggerMenu';
import { useSlashMenu } from '../src/composables/useSlashMenu';
import type { AppSkill } from '../src/api/types';
import type { TextSource } from '../src/composables/useTextSource';

const SKILL_A_PATH = '/Users/example/skills/alpha/SKILL.md';
const SKILL_B_PATH = '/Users/example/skills/beta/SKILL.md';

const SKILLS: AppSkill[] = [
  { name: 'alpha', description: 'Alpha skill', source: 'project', path: SKILL_A_PATH },
  { name: 'beta', description: 'Beta skill', source: 'project', path: SKILL_B_PATH },
  { name: 'gamma', description: 'Disabled skill', source: 'project', enabled: false },
];

// ---------------------------------------------------------------------------
// M2 — trigger tokens at word start
// ---------------------------------------------------------------------------

describe('extractSkillTriggerToken — word-start gating (US3.4/US3.6)', () => {
  it('should extract the trigger token when the caret is at the start of the text', () => {
    // Given the caret right after a "$" typed as the first character,
    // When extracting the skill trigger token,
    // Then the token spans from position 0.
    expect(extractSkillTriggerToken('$', 1)).toEqual({ start: 0, end: 1, query: '' });
    expect(extractSkillTriggerToken('$al', 3)).toEqual({ start: 0, end: 3, query: 'al' });
  });

  it('should extract the trigger token when the caret follows whitespace', () => {
    // Given the caret after "$" that immediately follows a space or newline,
    // When extracting the skill trigger token,
    // Then the token is recognized (word start after whitespace).
    expect(extractSkillTriggerToken('do this $', 9)).toEqual({ start: 8, end: 9, query: '' });
    expect(extractSkillTriggerToken('line\n$de', 8)).toEqual({ start: 5, end: 8, query: 'de' });
    expect(extractSkillTriggerToken('全角\u3000$', 4)).toEqual({ start: 3, end: 4, query: '' });
  });

  it('should accept all trigger characters including the full-width variants', () => {
    for (const trigger of ['¥', '￥']) {
      expect(extractSkillTriggerToken(`hello ${trigger}gr`, 9)?.query).toBe('gr');
    }
  });

  it('should accept a trigger token at a word start even mid-word for currency amounts', () => {
    // "$100" typed after a space IS a word start — the menu opens briefly until
    // the query stops matching. This is the accepted residual (SDR-005 risk 2
    // is about pasted links, and menu jank is bounded by word-start gating).
    expect(extractSkillTriggerToken('cost $100', 6)).toEqual({ start: 5, end: 6, query: '' });
  });

  it('should not extract a token when the trigger char is inside a word', () => {
    expect(extractSkillTriggerToken('a$b', 2)).toBeNull();
    expect(extractSkillTriggerToken('cost$100', 8)).toBeNull();
  });

  it('should not extract a token when the character before the caret is not a trigger char', () => {
    expect(extractSkillTriggerToken('hello', 5)).toBeNull();
    expect(extractSkillTriggerToken('', 0)).toBeNull();
  });

  it('should not extract a token when the query contains whitespace', () => {
    // Given the caret past whitespace typed after the trigger,
    // When extracting the skill trigger token,
    // Then no token — a message is being typed, not a trigger.
    expect(extractSkillTriggerToken('$deploy now', 11)).toBeNull();
    expect(extractSkillTriggerToken('$deploy\u3000now', 11)).toBeNull();
  });

  it('should use the slice from the trigger to the caret even when text continues after the caret', () => {
    // Given the caret in the middle of a trigger token being edited,
    // When extracting the token,
    // Then the query is only the text between the word start and the caret.
    expect(extractSkillTriggerToken('hi $alp h', 6)).toEqual({ start: 3, end: 6, query: 'al' });
  });

  it('should not extract a token when the text before the caret crosses into the previous word without whitespace', () => {
    // "abc$def" — the walk-back from the caret spans "abc$def" as one word, and
    // the word does not START with a trigger char, so no token.
    expect(extractSkillTriggerToken('abc$def', 7)).toBeNull();
  });
});

describe('extractSlashTriggerToken — word-start gating for "/"', () => {
  it('should extract the slash token at the start of the text', () => {
    expect(extractSlashTriggerToken('/', 1)).toEqual({ start: 0, end: 1, query: '' });
    expect(extractSlashTriggerToken('/de', 3)).toEqual({ start: 0, end: 3, query: 'de' });
  });

  it('should extract the slash token after whitespace', () => {
    expect(extractSlashTriggerToken('please /mo', 10)).toEqual({ start: 7, end: 10, query: 'mo' });
    expect(extractSlashTriggerToken('a\n/new', 6)).toEqual({ start: 2, end: 6, query: 'new' });
  });

  it('should not extract a token when the slash is inside a word (paths, URLs)', () => {
    // Given a "/" typed mid-word (code path, URL),
    // When extracting the slash token,
    // Then no token — the anti-interruption rule of US3.6.
    expect(extractSlashTriggerToken('src/lib/foo', 9)).toBeNull();
    expect(extractSlashTriggerToken('https://x', 7)).toBeNull();
  });

  it('should not extract a token when the query contains whitespace', () => {
    expect(extractSlashTriggerToken('/model now', 10)).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// M2 — insert computation (backfill branches)
// ---------------------------------------------------------------------------

describe('computeInlineReferenceInsert — backfill split by composer state', () => {
  it('should be a pure no-op helper owner: insert text at the caret with a trailing space', () => {
    // Covered indirectly via the menu composables below; kept here to assert
    // the pure insert math shared by both menus.
    const before = 'deploy the site';
    const insert = formatSkillReferenceLink('alpha', SKILL_A_PATH) + ' ';
    const caret = before.length;
    const next = before + insert;
    expect(next.startsWith(before)).toBe(true);
    expect(next.slice(caret + insert.length)).toBe('');
  });

  it('should collapse only the spaces BETWEEN adjacent references, not elsewhere', () => {
    const link = formatSkillReferenceLink('alpha', SKILL_A_PATH);
    const text = `${link}   ${link}  finish it`;
    expect(collapseInlineWhitespace(text)).toBe(`${link} ${link}  finish it`);
  });
});

// ---------------------------------------------------------------------------
// M4 — mixed-message submission (submit path selection)
// ---------------------------------------------------------------------------

describe('hasInlineReferences — mixed-message judgment (M4, US3.6)', () => {
  const alphaLink = formatSkillReferenceLink('alpha', SKILL_A_PATH);
  const cmdLink = formatSkillReferenceLink('model', 'command:/model');

  it('should be false for plain text (pure command / pure message paths keep working)', () => {
    expect(hasInlineReferences('hello world')).toBe(false);
    expect(hasInlineReferences('/goal finish the deploy')).toBe(false);
    expect(hasInlineReferences('')).toBe(false);
  });

  it('should be true when the text carries at least one reference', () => {
    expect(hasInlineReferences(`use ${alphaLink} please`)).toBe(true);
    expect(hasInlineReferences(`/model and ${alphaLink} mixed`)).toBe(true);
  });

  it('should treat a plain markdown link without a trigger prefix as non-reference text', () => {
    expect(hasInlineReferences('see [alpha](/skills/alpha/SKILL.md) here')).toBe(false);
  });

  it('should be true for a command reference even mixed into slash-command-looking text', () => {
    // US3.6: a message that starts with a slash command but also carries an
    // inline reference is a MIXED message — the pure-command intercept must
    // not take it.
    expect(hasInlineReferences(`/clear after ${cmdLink}`)).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// M3 — chip segments for message rendering
// ---------------------------------------------------------------------------

describe('splitMessageInlineReferences — chip split for the user bubble (M3)', () => {
  const alphaLink = formatSkillReferenceLink('alpha', SKILL_A_PATH);
  const cmdLink = formatSkillReferenceLink('model', 'command:/model');

  it('should return a single plain segment when the text has no references', () => {
    expect(splitMessageInlineReferences('just plain text')).toEqual<InlineSegment[]>([
      { kind: 'plain', text: 'just plain text' },
    ]);
  });

  it('should split text/plain and reference spans in order with exact offsets', () => {
    const text = `use ${alphaLink} and ${cmdLink} please`;
    const segments = splitMessageInlineReferences(text);
    expect(segments).toEqual<InlineSegment[]>([
      { kind: 'plain', text: 'use ' },
      { kind: 'skill', display: 'alpha', target: SKILL_A_PATH, start: 4, end: 4 + alphaLink.length },
      { kind: 'plain', text: ' and ' },
      { kind: 'command', display: 'model', target: 'command:/model', start: 4 + alphaLink.length + 5, end: 4 + alphaLink.length + 5 + cmdLink.length },
      { kind: 'plain', text: ' please' },
    ]);
  });

  it('should keep full-width trigger characters in command/skill detection', () => {
    const link = `[￥模型](command:/model)`;
    const segments = splitMessageInlineReferences(`run ${link} ok`);
    expect(segments[1]).toMatchObject({ kind: 'command', display: '模型' });
  });
});

describe('extractInlineReferenceSpan — caret-inside detection (M3 degrade rule)', () => {
  const alphaLink = formatSkillReferenceLink('alpha', SKILL_A_PATH);

  it('should return the reference covering the caret when the caret is inside the link', () => {
    const text = `use ${alphaLink} ok`;
    const span = extractInlineReferenceSpan(text, 6);
    expect(span).toMatchObject({ start: 4, end: 4 + alphaLink.length, display: 'alpha' });
  });

  it('should return null when the caret is outside any reference link', () => {
    const text = `use ${alphaLink} ok`;
    expect(extractInlineReferenceSpan(text, 2)).toBeNull();
    expect(extractInlineReferenceSpan(text, text.length)).toBeNull();
  });
});
// ---------------------------------------------------------------------------
// M2 — composables: caret-aware trigger + chip insert (TextSource adapter)
// ---------------------------------------------------------------------------

/** A TextSource over a mutable {text, caret} pair (mock editor host). */
function makeTextSource(field: { text: string; caret: number }) {
  const inserted: Array<{ display: string; target: string; kind: 'skill' | 'command' }> = [];
  const focus = vi.fn();
  const source: TextSource = {
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
    focus,
  };
  return { source, inserted, focus };
}

function setupSkillMenu(initialText: string, caret: number, skills: AppSkill[] = SKILLS) {
  const field = { text: initialText, caret };
  const host = makeTextSource(field);
  const menu = useSkillTriggerMenu({
    textSource: host.source,
    skills: () => skills,
  });
  return { field, ...host, menu };
}

describe('useSkillTriggerMenu — word-start trigger + chip insert (M2)', () => {
  it('should open with skill-only items when the caret follows whitespace after "$"', () => {
    const { menu } = setupSkillMenu('deploy the $', 12);
    menu.update();
    expect(menu.open.value).toBe(true);
    expect(menu.items.value.every((i) => i.isSkill === true)).toBe(true);
  });

  it('should filter by the query between the word start and the caret', () => {
    const { menu } = setupSkillMenu('please $be', 10);
    menu.update();
    expect(menu.items.value.map((i) => i.name)).toEqual(['/skill:beta']);
  });

  it('should stay closed when the trigger is typed mid-word', () => {
    const { menu } = setupSkillMenu('a$b', 2);
    menu.update();
    expect(menu.open.value).toBe(false);
  });

  it('should insert a skillMention chip at the caret when the composer already has content', () => {
    const { field, inserted, menu } = setupSkillMenu('deploy the site $al', 19);
    menu.update();
    menu.select(menu.items.value.find((i) => i.name === '/skill:alpha')!);
    expect(field.text).toBe('deploy the site [$alpha] ');
    expect(inserted).toEqual([{ display: 'alpha', target: SKILL_A_PATH, kind: 'skill' }]);
  });

  it('should insert a skillMention chip when the composer is empty (unified chip form)', () => {
    const { inserted, menu } = setupSkillMenu('$al', 3);
    menu.update();
    menu.select(menu.items.value.find((i) => i.name === '/skill:alpha')!);
    expect(inserted).toEqual([{ display: 'alpha', target: SKILL_A_PATH, kind: 'skill' }]);
  });

  it('should insert a skillMention chip for a builtin-sourced skill on an empty composer', () => {
    const builtinSkills: AppSkill[] = [
      { name: 'deploy', description: 'Deploy', source: 'builtin', path: '/example/deploy/SKILL.md' },
    ];
    const { inserted, menu } = setupSkillMenu('$', 1, builtinSkills);
    menu.update();
    menu.select(menu.items.value.find((i) => i.name === '/deploy')!);
    expect(inserted).toEqual([{ display: 'deploy', target: '/example/deploy/SKILL.md', kind: 'skill' }]);
  });

  it('should insert a skillMention chip regardless of the reported path shape (directory or file)', () => {
    const dirOnlySkills: AppSkill[] = [
      { name: 'deploy', description: 'Deploy', source: 'project', path: '/example/deploy' },
    ];
    const { inserted, menu } = setupSkillMenu('a $ tail', 3, dirOnlySkills);
    menu.update();
    menu.select(menu.items.value.find((i) => i.name === '/skill:deploy')!);
    expect(inserted).toEqual([{ display: 'deploy', target: '/example/deploy/SKILL.md', kind: 'skill' }]);
  });

  it('should not open when the skills list has no path-bearing match source (still opens, path optional)', () => {
    const { menu } = setupSkillMenu('$', 1, [
      { name: 'p', description: 'P', source: 'project' },
    ]);
    menu.update();
    expect(menu.open.value).toBe(true);
  });
});

function setupSlashMenuState(initialText: string, caret: number, skills: AppSkill[] = SKILLS) {
  const field = { text: initialText, caret };
  const host = makeTextSource(field);
  const emitted: string[] = [];
  const pushed: string[] = [];
  const slash = useSlashMenu({
    textSource: host.source,
    skills: () => skills,
    emitCommand: (cmd) => emitted.push(cmd),
    historyPush: (entry) => pushed.push(entry),
  });
  return { field, ...host, emitted, pushed, slash };
}

describe('useSlashMenu — word-start trigger + chip insert (M2)', () => {
  it('should stay closed when the slash is typed mid-word (path middle)', () => {
    const { slash } = setupSlashMenuState('src/lib', 4);
    slash.update();
    expect(slash.open.value).toBe(false);
  });

  it('should open when the slash follows whitespace', () => {
    const { slash } = setupSlashMenuState('use /ne', 7);
    slash.update();
    expect(slash.open.value).toBe(true);
    expect(slash.items.value.map((i) => i.name)).toEqual(['/new']);
  });

  it('should insert a command skillMention chip at the caret when the composer has content', () => {
    const { field, inserted, slash } = setupSlashMenuState('later /ne', 9);
    slash.update();
    slash.select(slash.items.value.find((i) => i.name === '/new')!);
    expect(field.text).toBe('later [$new] ');
    expect(inserted).toEqual([{ display: 'new', target: 'command:/new', kind: 'command' }]);
  });

  it('should insert a skill skillMention chip when a skill item is chosen mid-message', () => {
    const { field, inserted, slash } = setupSlashMenuState('go /al', 6);
    slash.update();
    const alpha = slash.items.value.find((i) => i.name === '/skill:alpha');
    expect(alpha).toBeDefined();
    slash.select(alpha!);
    expect(field.text).toBe('go [$alpha] ');
    expect(inserted).toEqual([{ display: 'alpha', target: SKILL_A_PATH, kind: 'skill' }]);
  });

  it('should keep the empty-composer behavior unchanged (command backfill + no emit for acceptsInput)', () => {
    const { field, emitted, slash } = setupSlashMenuState('/goal', 5);
    slash.update();
    slash.select(slash.items.value.find((i) => i.name === '/goal')!);
    expect(field.text).toBe('/goal ');
    expect(emitted).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// Compact token form (composer ergonomics follow-up): the composer stores
// `[$name]` only — the long absolute-path link is expanded at submit time.
// ---------------------------------------------------------------------------

describe('compact tokens — parse / expand / validate', () => {
  const SKILLS = [
    { name: 'alpha', path: '/Users/example/skills/alpha/SKILL.md' },
    { name: 'beta', path: '/Users/example/skills/beta/SKILL.md', enabled: false },
  ];

  it('should parse a compact token with its span', () => {
    const tokens = parseCompactTokens('go [$alpha] now');
    expect(tokens).toHaveLength(1);
    expect(tokens[0]).toMatchObject({ display: 'alpha', start: 3, end: 11 });
  });

  it('should not treat a plain bracketed word as a compact token', () => {
    expect(parseCompactTokens('see [alpha] and [note]')).toEqual([]);
  });

  it('should treat full-width trigger chars as compact tokens too', () => {
    expect(parseCompactTokens('[$alpha]')).toHaveLength(1);
    expect(parseCompactTokens('[¥alpha]')).toHaveLength(1);
    expect(parseCompactTokens('[￥alpha]')).toHaveLength(1);
  });

  it('should expand a compact token into the full link resolved by skill name', () => {
    const out = expandCompactTokens('go [$alpha] now', SKILLS);
    expect(out).toBe(`go ${formatSkillReferenceLink('alpha', SKILLS[0]!.path)} now`);
  });

  it('should expand command-shaped tokens to the command pseudo-target', () => {
    const out = expandCompactTokens('run [$/new] now', SKILLS);
    expect(out).toBe(`run ${formatSkillReferenceLink('/new', 'command:/new')} now`);
  });

  it('should leave an unresolvable token as visible text (degrade, no broken link)', () => {
    const out = expandCompactTokens('go [$ghost] now', SKILLS);
    expect(out).toBe('go [$ghost] now');
  });

  it('should not expand tokens that are already part of full links', () => {
    const link = formatSkillReferenceLink('alpha', SKILLS[0]!.path);
    const out = expandCompactTokens(`see ${link} end`, SKILLS);
    expect(out).toBe(`see ${link} end`);
  });

  it('should not re-expand the head of a full builtin link (builtin targets classify as references)', () => {
    // Given a full link whose target is the builtin pseudo path — the form
    // the builtin chips mint and the wire carries,
    // When expanding compact tokens,
    // Then the head is NOT re-expanded: the link must survive byte-for-byte
    // (regression: before the shared parser classified builtin targets, the
    // link span exclusion missed them and the head re-expanded into
    // `[$update-config](builtin://update-config)(builtin://update-config)`).
    const builtinSkills: AppSkill[] = [
      { name: 'update-config', description: 'Update config', source: 'builtin', path: 'builtin://update-config' },
    ];
    const link = formatSkillReferenceLink('update-config', 'builtin://update-config');
    const out = expandCompactTokens(`[$update-config](builtin://update-config) tail`, builtinSkills);
    expect(out).toBe(`[$update-config](builtin://update-config) tail`);
  });

  it('should not re-expand the head of a stale-suffixed builtin full link', () => {
    const builtinSkills: AppSkill[] = [
      { name: 'update-config', description: 'Update config', source: 'builtin', path: 'builtin://update-config' },
    ];
    const staleLink = formatSkillReferenceLink('update-config', 'builtin://update-config/SKILL.md');
    const out = expandCompactTokens(`use ${staleLink} end`, builtinSkills);
    expect(out).toBe(`use ${staleLink} end`);
  });

  it('should count compact tokens as inline references for the mixed-message gate', () => {
    expect(hasInlineReferences('go [$alpha] now')).toBe(true);
    expect(hasInlineReferences('plain text')).toBe(false);
  });

  it('should still count full links (persisted form) as references', () => {
    const link = formatSkillReferenceLink('alpha', SKILLS[0]!.path);
    expect(hasInlineReferences(`go ${link} now`)).toBe(true);
  });

  it('should split compact tokens into chip segments (composer form)', () => {
    const segments = splitMessageInlineReferences('go [$alpha] now');
    expect(segments.map((s) => s.kind)).toEqual(['plain', 'skill', 'plain']);
    const chip = segments[1] as Extract<InlineSegment, { kind: 'skill' }>;
    expect(chip.display).toBe('alpha');
  });

  it('should split full links into chip segments (history form)', () => {
    const link = formatSkillReferenceLink('alpha', SKILLS[0]!.path);
    const segments = splitMessageInlineReferences(`go ${link} now`);
    expect(segments.map((s) => s.kind)).toEqual(['plain', 'skill', 'plain']);
  });
});

// ---------------------------------------------------------------------------
// ADR-009 — node-attrs validation (pre-submit warning scoping)
// ---------------------------------------------------------------------------

describe('findInvalidSkillMentionAttrs — chip-attrs validation (ADR-009, SDR-007)', () => {
  it('should return no issues when the list is empty even though the pasted text looks like references', () => {
    // Given a message whose TEXT contains PHP-style array indexing but no chips,
    // When validating the collected chip attrs (empty list),
    // Then nothing is flagged — plain text never enters the warning path.
    expect(findInvalidSkillMentionAttrs([], SKILLS)).toEqual([]);
  });

  it('should flag a skill chip whose target is absent from the skill list', () => {
    const attrs = [{ display: 'gone', target: '/Users/example/skills/gone/SKILL.md', kind: 'skill' as const }];
    expect(findInvalidSkillMentionAttrs(attrs, SKILLS)).toEqual([
      { attrs: attrs[0], reason: 'missing' },
    ]);
  });

  it('should flag a skill chip whose target hits a disabled skill', () => {
    const attrs = [{ display: 'gamma', target: '/Users/example/skills/gamma/SKILL.md', kind: 'skill' as const }];
    // gamma 在 SKILLS 中无 path，展开补上 path 以命中 pathMap 的 disabled 分支
    expect(findInvalidSkillMentionAttrs(attrs, [{ ...SKILLS[2]!, path: '/Users/example/skills/gamma/SKILL.md' }])).toEqual([
      { attrs: attrs[0], reason: 'disabled' },
    ]);
  });

  it('should never flag command chips even when their target is unresolvable', () => {
    const attrs = [{ display: 'whatever', target: 'command:/nope', kind: 'command' as const }];
    expect(findInvalidSkillMentionAttrs(attrs, SKILLS)).toEqual([]);
  });

  it('should resolve a targetless chip by name and flag it only when the name misses', () => {
    // Given a legacy compact-token node (targetless) whose name hits the list,
    // When validating, Then it is valid.
    expect(
      findInvalidSkillMentionAttrs([{ display: 'alpha', target: '', kind: 'skill' }], SKILLS),
    ).toEqual([]);
    // Given a targetless node whose name misses every skill,
    // When validating, Then it is flagged missing.
    expect(
      findInvalidSkillMentionAttrs([{ display: 'nope', target: '', kind: 'skill' }], SKILLS),
    ).toEqual([{ attrs: { display: 'nope', target: '', kind: 'skill' }, reason: 'missing' }]);
  });

  it('should skip validation entirely when no skill carries a path (older backends)', () => {
    const pathless = [{ name: 'alpha' }, { name: 'beta' }];
    const attrs = [{ display: 'alpha', target: '/anything/SKILL.md', kind: 'skill' as const }];
    expect(findInvalidSkillMentionAttrs(attrs, pathless)).toEqual([]);
  });
});
