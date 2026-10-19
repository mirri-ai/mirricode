import { describe, expect, it } from 'vitest';

import {
  IPC_CHANNELS,
  OPEN_IN_APP_IDS,
  validateBadgeCountPayload,
  validateDeepLinkForwardPayload,
  validateExternalUrlPayload,
  validateNotifyPayload,
  validateOpenInAppPayload,
  validatePathPayload,
} from './ipc';

describe('IPC_CHANNELS', () => {
  it('should use the desktop: namespace for every channel', () => {
    for (const channel of Object.values(IPC_CHANNELS)) {
      expect(channel.startsWith('desktop:')).toBe(true);
    }
  });

  it('should be unique', () => {
    const values = Object.values(IPC_CHANNELS);
    expect(new Set(values).size).toBe(values.length);
  });
});

describe('OPEN_IN_APP_IDS', () => {
  it('should come from the registry and include the new apps when listed', () => {
    expect(OPEN_IN_APP_IDS).toContain('zed');
    expect(OPEN_IN_APP_IDS).toContain('warp');
    expect(OPEN_IN_APP_IDS).toContain('sublime');
  });
});

describe('validatePathPayload', () => {
  it('should accept a non-empty string path', () => {
    expect(validatePathPayload({ path: '/tmp/a.txt' })).toEqual({ path: '/tmp/a.txt' });
  });

  it('should pass through a positive line number for editor opens', () => {
    expect(validatePathPayload({ path: 'a.rs', line: 12 })).toEqual({ path: 'a.rs', line: 12 });
  });

  it('should reject a non-string path', () => {
    expect(validatePathPayload({ path: 42 })).toBeNull();
    expect(validatePathPayload({ path: null })).toBeNull();
  });

  it('should reject an empty or whitespace-only path', () => {
    expect(validatePathPayload({ path: '' })).toBeNull();
    expect(validatePathPayload({ path: '   ' })).toBeNull();
  });

  it('should reject a path containing a NUL byte', () => {
    expect(validatePathPayload({ path: 'a\0b' })).toBeNull();
  });

  it('should reject a path over the length cap', () => {
    expect(validatePathPayload({ path: 'a'.repeat(4097) })).toBeNull();
  });

  it('should reject a non-positive line number', () => {
    expect(validatePathPayload({ path: 'a.rs', line: 0 })).toBeNull();
    expect(validatePathPayload({ path: 'a.rs', line: -1 })).toBeNull();
    expect(validatePathPayload({ path: 'a.rs', line: '12' })).toBeNull();
  });

  it('should reject a non-object payload', () => {
    expect(validatePathPayload(undefined)).toBeNull();
    expect(validatePathPayload('a.rs')).toBeNull();
    expect(validatePathPayload(null)).toBeNull();
  });
});

describe('validateOpenInAppPayload', () => {
  it('should accept a known app id with a path', () => {
    expect(validateOpenInAppPayload({ appId: 'vscode', path: '/ws' })).toEqual({
      appId: 'vscode',
      path: '/ws',
    });
  });

  it('should reject an unknown app id', () => {
    expect(validateOpenInAppPayload({ appId: 'rm -rf', path: '/ws' })).toBeNull();
  });

  it('should reject a bad path', () => {
    expect(validateOpenInAppPayload({ appId: 'vscode', path: '' })).toBeNull();
  });
});

describe('validateExternalUrlPayload', () => {
  it('should accept http and https URLs (normalized by URL)', () => {
    expect(validateExternalUrlPayload({ url: 'http://example.com' })).toBe('http://example.com/');
    expect(validateExternalUrlPayload({ url: 'https://github.com/a/b' })).toBe(
      'https://github.com/a/b',
    );
  });

  it('should reject non-http(s) schemes', () => {
    expect(validateExternalUrlPayload({ url: 'file:///etc/passwd' })).toBeNull();
    expect(validateExternalUrlPayload({ url: 'javascript:alert(1)' })).toBeNull();
    expect(validateExternalUrlPayload({ url: 'mirricode://session/x' })).toBeNull();
  });

  it('should reject garbage', () => {
    expect(validateExternalUrlPayload({ url: 'not a url' })).toBeNull();
    expect(validateExternalUrlPayload(undefined)).toBeNull();
  });
});

describe('validateNotifyPayload', () => {
  it('should accept a title/body pair with an optional tag', () => {
    expect(validateNotifyPayload({ title: 'Turn finished', body: 'hello' })).toEqual({
      title: 'Turn finished',
      body: 'hello',
      tag: undefined,
    });
    expect(validateNotifyPayload({ title: 'a', body: 'b', tag: 'mirri-complete-1' })).toEqual({
      title: 'a',
      body: 'b',
      tag: 'mirri-complete-1',
    });
  });

  it('should reject missing or non-string fields', () => {
    expect(validateNotifyPayload({ title: '', body: 'b' })).toBeNull();
    expect(validateNotifyPayload({ title: 'a' })).toBeNull();
    expect(validateNotifyPayload(undefined)).toBeNull();
  });

  it('should enforce length caps', () => {
    expect(validateNotifyPayload({ title: 'a'.repeat(201), body: 'b' })).toBeNull();
    expect(validateNotifyPayload({ title: 'a', body: 'b'.repeat(501) })).toBeNull();
  });
});

describe('validateBadgeCountPayload', () => {
  it('should accept a non-negative integer', () => {
    expect(validateBadgeCountPayload({ count: 0 })).toBe(0);
    expect(validateBadgeCountPayload({ count: 3 })).toBe(3);
  });

  it('should reject negatives, floats, and non-numbers', () => {
    expect(validateBadgeCountPayload({ count: -1 })).toBeNull();
    expect(validateBadgeCountPayload({ count: 1.5 })).toBeNull();
    expect(validateBadgeCountPayload({ count: '3' })).toBeNull();
    expect(validateBadgeCountPayload(undefined)).toBeNull();
  });
});

describe('validateDeepLinkForwardPayload', () => {
  it('should accept a mirricode:// url string', () => {
    expect(validateDeepLinkForwardPayload('mirricode://session/abc')).toBe(
      'mirricode://session/abc',
    );
  });

  it('should reject other schemes and non-strings', () => {
    expect(validateDeepLinkForwardPayload('https://example.com')).toBeNull();
    expect(validateDeepLinkForwardPayload(42)).toBeNull();
  });
});
