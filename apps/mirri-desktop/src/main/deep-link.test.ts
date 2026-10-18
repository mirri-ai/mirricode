import { describe, expect, it } from 'vitest';

import { extractDeepLinks, parseDeepLink } from './deep-link';

describe('parseDeepLink', () => {
  it('should parse a session deep link', () => {
    expect(parseDeepLink('mirricode://session/abc-123')).toEqual({
      type: 'session',
      sessionId: 'abc-123',
    });
  });

  it('should decode a url-encoded session id', () => {
    expect(parseDeepLink('mirricode://session/a%20b')).toEqual({
      type: 'session',
      sessionId: 'a b',
    });
  });

  it('should reject a session id containing a slash after decoding', () => {
    expect(parseDeepLink('mirricode://session/a%2Fb')).toBeNull();
  });

  it('should parse the plain open action', () => {
    expect(parseDeepLink('mirricode://open')).toEqual({ type: 'open' });
  });

  it('should treat a bare scheme url as a plain open', () => {
    expect(parseDeepLink('mirricode://')).toEqual({ type: 'open' });
  });

  it('should ignore unknown hosts and paths', () => {
    expect(parseDeepLink('mirricode://settings/foo')).toBeNull();
    expect(parseDeepLink('mirricode://session/')).toBeNull();
    expect(parseDeepLink('mirricode://session/a/b')).toBeNull();
  });

  it('should ignore non-mirricode urls', () => {
    expect(parseDeepLink('https://mirri.ai')).toBeNull();
    expect(parseDeepLink('mirricode:session/abc')).toBeNull();
  });

  it('should ignore a session id over the length cap', () => {
    expect(parseDeepLink(`mirricode://session/${'a'.repeat(257)}`)).toBeNull();
  });

  it('should never throw on garbage', () => {
    expect(parseDeepLink('::::')).toBeNull();
    expect(parseDeepLink('')).toBeNull();
  });
});

describe('extractDeepLinks', () => {
  it('should pick mirricode:// urls out of an argv array', () => {
    expect(
      extractDeepLinks([
        '/Applications/Mirri Code Desktop.app/Contents/MacOS/Mirri Code Desktop',
        '--flag',
        'mirricode://session/abc',
        'positional.txt',
      ]),
    ).toEqual(['mirricode://session/abc']);
  });

  it('should return an empty array when none are present', () => {
    expect(extractDeepLinks(['electron', '--no-sandbox'])).toEqual([]);
  });

  it('should keep multiple deep links in order', () => {
    expect(extractDeepLinks(['mirricode://open', 'x', 'mirricode://session/b'])).toEqual([
      'mirricode://open',
      'mirricode://session/b',
    ]);
  });
});
