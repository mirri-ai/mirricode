import { describe, expect, it } from 'vitest';

import {
  SKILL_TRIGGER_CHARS,
  collapseInlineWhitespace,
  formatSkillReferenceLink,
  parseInlineReferences,
} from '../skill-reference';

const SKILL_A = '/Users/example/skills/alpha/SKILL.md';
const SKILL_B = '/Users/example/skills/beta/SKILL.md';

describe('parseInlineReferences', () => {
  it('should parse nothing when the text contains no links', () => {
    const result = parseInlineReferences('plain test with no links');
    expect(result.references).toEqual([]);
    expect(result.body).toBe('plain test with no links');
  });

  it('should parse a skill reference link anywhere in the text', () => {
    const text = `plain test [$alpha](${SKILL_A}) trailing`;
    const result = parseInlineReferences(text);
    expect(result.references).toHaveLength(1);
    expect(result.references[0]).toMatchObject({
      kind: 'skill',
      display: 'alpha',
      target: SKILL_A,
      start: 11,
      end: 11 + `[$alpha](${SKILL_A})`.length,
    });
    expect(result.body).toBe('plain test [alpha] trailing');
  });

  it('should parse multiple skill references in order', () => {
    const text = `[$alpha](${SKILL_A}) and [$beta](${SKILL_B}) do the work`;
    const result = parseInlineReferences(text);
    expect(result.references.map((r) => r.display)).toEqual(['alpha', 'beta']);
    const firstLen = `[$alpha](${SKILL_A})`.length;
    expect(result.references.map((r) => r.start)).toEqual([0, firstLen + ' and '.length]);
    expect(result.body).toBe('[alpha] and [beta] do the work');
  });

  it('should accept all trigger characters including full-width variants', () => {
    for (const trigger of SKILL_TRIGGER_CHARS) {
      const text = `x [${trigger}alpha](${SKILL_A}) y`;
      const result = parseInlineReferences(text);
      expect(result.references).toHaveLength(1);
      expect(result.references[0]?.display).toBe('alpha');
    }
  });

  it('should treat plain markdown links without a trigger prefix as normal text', () => {
    const text = `see [alpha](${SKILL_A}) for details`;
    const result = parseInlineReferences(text);
    expect(result.references).toEqual([]);
    expect(result.body).toBe(text);
  });

  it('should treat a trigger character that is not a link as normal text', () => {
    const result = parseInlineReferences('cost is $100 and $ARGUMENTS stays');
    expect(result.references).toEqual([]);
    expect(result.body).toBe('cost is $100 and $ARGUMENTS stays');
  });

  it('should keep skill references with non-file targets as normal links', () => {
    const text = `docs [${'$'}guide](https://example.com/skills) here`;
    const result = parseInlineReferences(text);
    expect(result.references).toEqual([]);
    expect(result.body).toBe(text);
  });

  it('should parse a command reference with a command: target', () => {
    const text = `run [$clear](command:/clear) after`;
    const result = parseInlineReferences(text);
    expect(result.references).toHaveLength(1);
    expect(result.references[0]).toMatchObject({
      kind: 'command',
      display: 'clear',
      target: 'command:/clear',
    });
    expect(result.body).toBe('run [clear] after');
  });

  it('should collapse duplicate references to the same resolved skill by path', () => {
    const text = `[$alpha](${SKILL_A}) then [$alpha](${SKILL_A}) again`;
    const result = parseInlineReferences(text);
    expect(result.references).toHaveLength(2);
    const unique = [...new Set(result.references.map((r) => r.target))];
    expect(unique).toEqual([SKILL_A]);
  });

  it('should not collapse same-name references pointing to different paths', () => {
    const text = `[$alpha](${SKILL_A}) vs [$alpha](${SKILL_B})`;
    const result = parseInlineReferences(text);
    const targets = result.references.map((r) => r.target);
    expect(targets).toEqual([SKILL_A, SKILL_B]);
  });

  it('should replace the reference text with the display name without the trigger char', () => {
    const result = parseInlineReferences(`[$alpha](${SKILL_A})`);
    expect(result.body).toBe('[alpha]');
  });

  it('should be idempotent: re-parsing the body yields no references', () => {
    const first = parseInlineReferences(`a [$alpha](${SKILL_A}) b`);
    const second = parseInlineReferences(first.body);
    expect(second.references).toEqual([]);
  });

  it('should classify a builtin pseudo-path target as a skill reference', () => {
    const text = 'use [$update-config](builtin://update-config) now';
    const result = parseInlineReferences(text);
    expect(result.references).toHaveLength(1);
    expect(result.references[0]).toMatchObject({
      kind: 'skill',
      display: 'update-config',
      target: 'builtin://update-config',
    });
    expect(result.body).toBe('use [update-config] now');
  });

  it('should classify and normalize a stale builtin target carrying a /SKILL.md suffix', () => {
    // Historical persisted messages carry the dirty `builtin://name/SKILL.md`
    // form; the parser must classify it AND normalize the target.
    const dirty = 'use [$update-config](builtin://update-config/SKILL.md) now';
    const result = parseInlineReferences(dirty);
    expect(result.references).toHaveLength(1);
    expect(result.references[0]).toMatchObject({
      kind: 'skill',
      display: 'update-config',
      target: 'builtin://update-config',
    });
    expect(result.body).toBe('use [update-config] now');
  });

  it('should normalize the stale builtin suffix case-insensitively', () => {
    const result = parseInlineReferences('[$update-config](builtin://update-config/skill.MD)');
    expect(result.references[0]?.target).toBe('builtin://update-config');
  });

  it('should keep slash-shaped builtin targets untouched when they carry no stale suffix', () => {
    // The sub-skill family legitimately uses slash-separated pseudo paths
    // (`builtin://sub-skill/review`) — only the exact trailing `/SKILL.md`
    // (case-insensitive) marks the dirty historical form.
    const result = parseInlineReferences('[$review](builtin://sub-skill/review)');
    expect(result.references[0]?.target).toBe('builtin://sub-skill/review');
  });

  it('should keep disk path targets unchanged when classified', () => {
    const result = parseInlineReferences(`[$alpha](${SKILL_A})`);
    expect(result.references[0]?.target).toBe(SKILL_A);
    // Case-insensitive disk extension stays intact too
    const upper = parseInlineReferences('[$alpha](/a/SKILL.MD)');
    expect(upper.references[0]?.target).toBe('/a/SKILL.MD');
  });

  it('should not classify a builtin-ish target without the trigger prefix', () => {
    const result = parseInlineReferences('see [update-config](builtin://update-config)');
    expect(result.references).toEqual([]);
    expect(result.body).toBe('see [update-config](builtin://update-config)');
  });
});

describe('collapseInlineWhitespace', () => {
  it('should collapse consecutive spaces between references into one separator', () => {
    const text = `[$alpha](${SKILL_A})   [$beta](${SKILL_B})`;
    expect(collapseInlineWhitespace(text)).toBe(`[$alpha](${SKILL_A}) [$beta](${SKILL_B})`);
  });

  it('should not touch spaces inside code spans or ordinary words', () => {
    expect(collapseInlineWhitespace('a  b   c')).toBe('a  b   c');
    expect(collapseInlineWhitespace('x    y')).toBe('x    y');
  });

  it('should trim leading and trailing whitespace', () => {
    const text = `  [$alpha](${SKILL_A})  `;
    expect(collapseInlineWhitespace(text)).toBe(`[$alpha](${SKILL_A})`);
  });
});

describe('formatSkillReferenceLink', () => {
  it('should format a skill reference as a trigger-prefixed inline link', () => {
    expect(formatSkillReferenceLink('alpha', SKILL_A)).toBe(`[$alpha](${SKILL_A})`);
  });

  it('should format a command reference with a command: pseudo-target', () => {
    expect(formatSkillReferenceLink('clear', 'command:/clear')).toBe(`[$clear](command:/clear)`);
  });

  it('should escape square brackets in the display name', () => {
    expect(formatSkillReferenceLink('a]b', SKILL_A)).toBe(`[$a\\]b](${SKILL_A})`);
  });

  it('should round-trip: parse(format(x)) recovers the reference', () => {
    const link = formatSkillReferenceLink('alpha', SKILL_A);
    const result = parseInlineReferences(link);
    expect(result.references).toHaveLength(1);
    const ref = result.references[0]!;
    expect(ref.display).toBe('alpha');
    expect(ref.target).toBe(SKILL_A);
  });
});
