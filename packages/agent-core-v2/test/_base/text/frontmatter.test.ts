import { describe, expect, it } from 'vitest';

import { FrontmatterError, parseFrontmatter, serializeFrontmatter } from '#/_base/text/frontmatter';

describe('parseFrontmatter', () => {
  it('parses yaml frontmatter and body', () => {
    const { data, body } = parseFrontmatter('---\nname: foo\n---\nbody text');
    expect(data).toEqual({ name: 'foo' });
    expect(body).toBe('body text');
  });

  it('returns null data when there is no frontmatter', () => {
    const { data, body } = parseFrontmatter('just body');
    expect(data).toBeNull();
    expect(body).toBe('just body');
  });

  it('throws when the closing fence is missing', () => {
    expect(() => parseFrontmatter('---\nname: foo')).toThrow(FrontmatterError);
  });
});

describe('serializeFrontmatter', () => {
  it('should produce valid frontmatter with body when given data and body', () => {
    const text = serializeFrontmatter({ name: 'reviewer', description: 'Code reviewer' }, 'You are a code reviewer.');
    const { data, body } = parseFrontmatter(text);
    expect(data).toEqual({ name: 'reviewer', description: 'Code reviewer' });
    expect(body).toBe('You are a code reviewer.');
  });

  it('should round-trip parse → serialize → parse with identical data and body', () => {
    const original = '---\nname: coder\ndescription: Code agent\ntools:\n  - Read\n  - Write\n---\nYou write code.';
    const parsed = parseFrontmatter(original);
    const serialized = serializeFrontmatter(parsed.data as Record<string, unknown>, parsed.body);
    const reparsed = parseFrontmatter(serialized);
    expect(reparsed.data).toEqual(parsed.data);
    expect(reparsed.body).toBe(parsed.body);
  });

  it('should produce a leading frontmatter fence even when body is empty', () => {
    const text = serializeFrontmatter({ name: 'minimal' }, '');
    expect(text.startsWith('---\n')).toBe(true);
    const { data, body } = parseFrontmatter(text);
    expect(data).toEqual({ name: 'minimal' });
    expect(body).toBe('');
  });

  it('should handle empty data object as valid frontmatter', () => {
    const text = serializeFrontmatter({}, 'just a prompt');
    const { data, body } = parseFrontmatter(text);
    expect(data).toEqual({});
    expect(body).toBe('just a prompt');
  });
});
