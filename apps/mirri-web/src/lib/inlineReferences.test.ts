import { describe, expect, it } from 'vitest';
import { splitMessageInlineReferences } from './inlineReferences';

describe('splitMessageInlineReferences with with_skills (ADR-006)', () => {
  const skills = [{ name: 'review', target: '/skills/review/SKILL.md', trigger: 'user-inline' }];

  it('should rebuild a bare-name marker as a skill chip when with_skills matches', () => {
    const segs = splitMessageInlineReferences('[review] the code', skills);
    expect(segs).toEqual([
      { kind: 'skill', display: 'review', target: '/skills/review/SKILL.md', start: 0, end: 8 },
      { kind: 'plain', text: ' the code' },
    ]);
  });

  it('should leave unmatched bare-name markers as plain text', () => {
    const segs = splitMessageInlineReferences('[unknown] stays text', skills);
    expect(segs).toEqual([{ kind: 'plain', text: '[unknown] stays text' }]);
  });

  it('should keep current behavior when with_skills is absent (legacy messages)', () => {
    const segs = splitMessageInlineReferences('[review] the code');
    expect(segs).toEqual([{ kind: 'plain', text: '[review] the code' }]);
  });

  it('should rebuild every marker of a repeated skill to the same chip', () => {
    const segs = splitMessageInlineReferences('[review] then [review] again', skills);
    const chips = segs.filter(
      (s): s is { kind: 'skill'; display: string; target: string; start: number; end: number } =>
        s.kind === 'skill',
    );
    expect(chips).toHaveLength(2);
    expect(chips.every((s) => s.target === '/skills/review/SKILL.md')).toBe(true);
  });

  it('should not let a with_skills name shadow trigger-prefixed forms (full links still win)', () => {
    const segs = splitMessageInlineReferences('[$review](/skills/review/SKILL.md) ok', skills);
    expect(segs[0]!.kind).toBe('skill');
    expect(segs[0]).toMatchObject({ display: 'review', target: '/skills/review/SKILL.md' });
  });

  it('should split a builtin full link into a skill chip (builtin targets classify as references)', () => {
    // The shared parser classifies `builtin://` targets (ADR-007 D4), so the
    // chip-split plan turns a builtin link into a skill segment — the same
    // path the disk-path links take.
    const segs = splitMessageInlineReferences(
      'use [$update-config](builtin://update-config) now',
    );
    expect(segs[0]).toEqual({ kind: 'plain', text: 'use ' });
    expect(segs[1]).toMatchObject({
      kind: 'skill',
      display: 'update-config',
      target: 'builtin://update-config',
    });
    expect(segs[2]).toEqual({ kind: 'plain', text: ' now' });
  });

  it('should split a stale-suffixed builtin link with the normalized target', () => {
    const segs = splitMessageInlineReferences(
      'use [$update-config](builtin://update-config/SKILL.md) now',
    );
    expect(segs[0]).toEqual({ kind: 'plain', text: 'use ' });
    expect(segs[1]).toMatchObject({ kind: 'skill', target: 'builtin://update-config' });
    expect(segs[2]).toEqual({ kind: 'plain', text: ' now' });
  });
});
