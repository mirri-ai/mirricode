import { describe, expect, it } from 'vitest';

import { sanitizeAttachmentName } from '../src/routes/prompts';

describe('sanitizeAttachmentName', () => {
  it('preserves a normal filename', () => {
    expect(sanitizeAttachmentName('report.pdf')).toBe('report.pdf');
    expect(sanitizeAttachmentName('data.json')).toBe('data.json');
  });

  it('replaces path separators with underscore', () => {
    expect(sanitizeAttachmentName('../etc/passwd')).toBe('_etc_passwd');
    expect(sanitizeAttachmentName('a/b/c.txt')).toBe('a_b_c.txt');
    expect(sanitizeAttachmentName('a\\b\\c.txt')).toBe('a_b_c.txt');
  });

  it('strips leading dots so the file is not hidden', () => {
    expect(sanitizeAttachmentName('.env')).toBe('env');
    expect(sanitizeAttachmentName('...secret')).toBe('secret');
    expect(sanitizeAttachmentName('..')).toBe('attachment');
  });

  it('removes control characters', () => {
    expect(sanitizeAttachmentName('file\u0000name')).toBe('filename');
    expect(sanitizeAttachmentName('file\x1Fname')).toBe('filename');
    expect(sanitizeAttachmentName('file\u007Fname')).toBe('filename');
    expect(sanitizeAttachmentName('file\nname')).toBe('filename');
  });

  it('caps the length at 100 characters', () => {
    const long = 'a'.repeat(150);
    const result = sanitizeAttachmentName(long);
    expect(result.length).toBe(100);
  });

  it('falls back to "attachment" for empty or whitespace-only names', () => {
    expect(sanitizeAttachmentName('')).toBe('attachment');
    expect(sanitizeAttachmentName('   ')).toBe('attachment');
    expect(sanitizeAttachmentName('...')).toBe('attachment');
  });

  it('preserves file extensions after sanitization', () => {
    expect(sanitizeAttachmentName('./../image.png')).toBe('_.._image.png');
  });

  it('handles a mix of path traversal and control characters', () => {
    const malicious = '..\\..\\..\\etc\u0000shadow';
    expect(sanitizeAttachmentName(malicious)).toBe('_.._.._etcshadow');
  });
});
