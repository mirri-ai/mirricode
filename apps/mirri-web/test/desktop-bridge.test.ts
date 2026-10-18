// apps/mirri-web/test/desktop-bridge.test.ts
//
// Pure helpers of the desktop bridge module: workspace-path resolution,
// deep-link parsing, and the structural bridge detection. The three-state
// openFile degrade chain itself is covered in ws-rpc-desktop-bridge.test.ts
// (it needs the WsRpcApi harness).

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  ExternalOpenUnavailableError,
  getDesktopBridge,
  isExternalOpenUnavailableError,
  parseSessionDeepLink,
  resolveWorkspacePath,
} from '../src/lib/desktopBridge';

// mirri-web's vitest suite runs in the node environment; the bridge reads
// `window` lazily at call time, so a stubbed global serves the detector.
beforeEach(() => {
  vi.stubGlobal('window', {});
});

afterEach(() => {
  vi.unstubAllGlobals();
});

function installBridge(partial: Record<string, unknown>): void {
  (window as unknown as { mirriDesktop?: unknown }).mirriDesktop = partial;
}

describe('getDesktopBridge', () => {
  afterEach(() => {
    delete (window as unknown as { mirriDesktop?: unknown }).mirriDesktop;
  });
  it('should return null when no bridge is exposed (browser)', () => {
    expect(getDesktopBridge()).toBeNull();
  });

  it('should return the bridge when the full surface is present', () => {
    installBridge({
      openPath: () => Promise.resolve({ opened: true }),
      revealPath: () => Promise.resolve({ revealed: true }),
      openInEditor: () => Promise.resolve({ opened: true }),
      openInApp: () => Promise.resolve({ opened: true }),
      openExternal: () => Promise.resolve({ opened: true }),
      notify: () => Promise.resolve({ shown: true }),
      setBadgeCount: () => Promise.resolve({ set: true }),
      dockBounce: () => Promise.resolve(),
      setLoginItem: () => Promise.resolve({ open: true }),
      getLoginItem: () => Promise.resolve({ open: false }),
      onDeepLink: () => {},
      onNotificationClick: () => {},
    });
    expect(getDesktopBridge()).not.toBeNull();
  });

  it('should reject a partial (stale) bridge structurally', () => {
    installBridge({ openPath: () => Promise.resolve({ opened: true }) });
    expect(getDesktopBridge()).toBeNull();
  });
});

describe('resolveWorkspacePath', () => {
  it('should join a relative path onto the cwd', () => {
    expect(resolveWorkspacePath('/home/u/ws', 'src/a.rs')).toBe('/home/u/ws/src/a.rs');
  });

  it('should not double the slash when the cwd ends with one', () => {
    expect(resolveWorkspacePath('/home/u/ws/', 'a.rs')).toBe('/home/u/ws/a.rs');
  });

  it('should pass absolute POSIX paths through unchanged', () => {
    expect(resolveWorkspacePath('/home/u/ws', '/etc/hosts')).toBe('/etc/hosts');
  });

  it('should pass Windows drive-letter paths through', () => {
    expect(resolveWorkspacePath('C:\\ws', 'C:\\other\\a.rs')).toBe('C:\\other\\a.rs');
  });
});

describe('parseSessionDeepLink', () => {
  it('should extract a session id', () => {
    expect(parseSessionDeepLink('mirricode://session/abc-123')).toBe('abc-123');
  });

  it('should decode url-encoded ids', () => {
    expect(parseSessionDeepLink('mirricode://session/a%20b')).toBe('a b');
  });

  it('should reject non-session urls and garbage', () => {
    expect(parseSessionDeepLink('mirricode://open')).toBeNull();
    expect(parseSessionDeepLink('https://mirri.ai')).toBeNull();
    expect(parseSessionDeepLink('mirricode://session/')).toBeNull();
    expect(parseSessionDeepLink('mirricode://session/a/b')).toBeNull();
    expect(parseSessionDeepLink('')).toBeNull();
  });
});

describe('ExternalOpenUnavailableError', () => {
  it('should carry the operation and target for the degrade prompt', () => {
    const err = new ExternalOpenUnavailableError('openFile', '/ws/a.rs');
    expect(err.name).toBe('ExternalOpenUnavailableError');
    expect(err.target).toBe('/ws/a.rs');
    expect(isExternalOpenUnavailableError(err)).toBe(true);
  });

  it('should not match ordinary errors', () => {
    expect(isExternalOpenUnavailableError(new Error('nope'))).toBe(false);
    expect(isExternalOpenUnavailableError('nope')).toBe(false);
  });
});
