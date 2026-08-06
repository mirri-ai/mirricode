import { describe, it, expect } from 'vitest';
import { parseFrontmatter, serializeFrontmatter, FrontmatterError } from '#/frontmatter';

describe('parseFrontmatter', () => {
  it('should parse frontmatter and body when given a valid markdown document', () => {
    const text = '---\nname: reviewer\ndescription: Test\n---\nYou are a reviewer.';
    const result = parseFrontmatter(text);
    expect(result.data).toEqual({ name: 'reviewer', description: 'Test' });
    expect(result.body).toBe('You are a reviewer.');
  });

  it('should return null data and full text as body when document has no frontmatter fence', () => {
    const text = 'Just some text.';
    const result = parseFrontmatter(text);
    expect(result.data).toBeNull();
    expect(result.body).toBe('Just some text.');
  });

  it('should throw FrontmatterError when closing fence is missing', () => {
    const text = '---\nname: reviewer\n';
    expect(() => parseFrontmatter(text)).toThrow(FrontmatterError);
  });

  it('should return empty object data when frontmatter is empty', () => {
    const text = '---\n---\nBody here.';
    const result = parseFrontmatter(text);
    expect(result.data).toEqual({});
    expect(result.body).toBe('Body here.');
  });
});

describe('serializeFrontmatter', () => {
  it('should produce frontmatter and body when given data and non-empty body', () => {
    const text = serializeFrontmatter({ name: 'reviewer' }, 'You are a reviewer.');
    expect(text).toContain('---');
    expect(text).toContain('name: reviewer');
    expect(text).toContain('You are a reviewer.');
  });

  it('should produce closing fence only when body is empty', () => {
    const text = serializeFrontmatter({ name: 'reviewer' }, '');
    expect(text.trimEnd()).toMatch(/^---\n[\s\S]*\n---$/);
    expect(text).not.toContain('You are');
  });

  it('should round-trip: parse → serialize → parse preserves data and body', () => {
    const original = '---\nname: reviewer\ndescription: Test\n---\nYou are a reviewer.';
    const parsed = parseFrontmatter(original);
    const serialized = serializeFrontmatter(parsed.data as Record<string, unknown>, parsed.body);
    const reparsed = parseFrontmatter(serialized);
    expect(reparsed.data).toEqual(parsed.data);
    expect(reparsed.body).toBe(parsed.body);
  });
});
