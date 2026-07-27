import { describe, expect, it } from 'vitest';

import { decideWillNavigate, decideWindowOpen, isExternalHttpUrl } from './external-links';

describe('isExternalHttpUrl', () => {
  it('should return true when URL starts with http://', () => {
    expect(isExternalHttpUrl('http://example.com')).toBe(true);
  });

  it('should return true when URL starts with https://', () => {
    expect(isExternalHttpUrl('https://github.com/owner/repo')).toBe(true);
  });

  it('should return false when URL starts with data:', () => {
    expect(isExternalHttpUrl('data:text/html,<h1>hello</h1>')).toBe(false);
  });

  it('should return false when URL starts with file://', () => {
    expect(isExternalHttpUrl('file:///etc/passwd')).toBe(false);
  });

  it('should return false when URL starts with javascript:', () => {
    expect(isExternalHttpUrl('javascript:alert(1)')).toBe(false);
  });

  it('should return false when URL starts with mailto:', () => {
    expect(isExternalHttpUrl('mailto:user@example.com')).toBe(false);
  });

  it('should return false for empty string', () => {
    expect(isExternalHttpUrl('')).toBe(false);
  });
});

describe('decideWindowOpen', () => {
  it('should deny and open externally when URL is https', () => {
    const result = decideWindowOpen('https://github.com/owner/repo');
    expect(result.action).toBe('deny');
    expect(result.openExternally).toBe(true);
  });

  it('should deny and open externally when URL is http', () => {
    const result = decideWindowOpen('http://example.com');
    expect(result.action).toBe('deny');
    expect(result.openExternally).toBe(true);
  });

  it('should deny and not open externally when URL is data:', () => {
    const result = decideWindowOpen('data:text/html,<h1>loading</h1>');
    expect(result.action).toBe('deny');
    expect(result.openExternally).toBe(false);
  });

  it('should deny and not open externally when URL is file://', () => {
    const result = decideWindowOpen('file:///etc/passwd');
    expect(result.action).toBe('deny');
    expect(result.openExternally).toBe(false);
  });

  it('should deny and not open externally when URL is javascript:', () => {
    const result = decideWindowOpen('javascript:alert(1)');
    expect(result.action).toBe('deny');
    expect(result.openExternally).toBe(false);
  });

  it('should always deny the in-app window regardless of URL', () => {
    for (const url of ['https://github.com', 'data:text/html,x', '', 'mailto:a@b.c']) {
      expect(decideWindowOpen(url).action).toBe('deny');
    }
  });
});

describe('decideWillNavigate', () => {
  const origin = 'http://127.0.0.1:58627';

  it('should allow same-origin navigation', () => {
    const result = decideWillNavigate('http://127.0.0.1:58627/api/v1/healthz', origin);
    expect(result.preventDefault).toBe(false);
    expect(result.openExternally).toBe(false);
  });

  it('should block and open externally when navigating to https external URL', () => {
    const result = decideWillNavigate('https://github.com/owner/repo', origin);
    expect(result.preventDefault).toBe(true);
    expect(result.openExternally).toBe(true);
  });

  it('should block and open externally when navigating to http external URL', () => {
    const result = decideWillNavigate('http://example.com', origin);
    expect(result.preventDefault).toBe(true);
    expect(result.openExternally).toBe(true);
  });

  it('should block but not open externally when navigating to file:// URL', () => {
    const result = decideWillNavigate('file:///etc/passwd', origin);
    expect(result.preventDefault).toBe(true);
    expect(result.openExternally).toBe(false);
  });

  it('should block but not open externally when URL is invalid', () => {
    const result = decideWillNavigate('not-a-url', origin);
    expect(result.preventDefault).toBe(true);
    expect(result.openExternally).toBe(false);
  });

  it('should allow same-origin with different port', () => {
    // Different port = different origin, so it should be blocked.
    const result = decideWillNavigate('http://127.0.0.1:58827/path', origin);
    expect(result.preventDefault).toBe(true);
    expect(result.openExternally).toBe(true);
  });
});
