// apps/mirri-web/test/ws-rpc-desktop-bridge.test.ts
//
// The three-state degrade chain for external file opens (D3/D8 seam):
//   1. desktop bridge (window.mirriDesktop via IPC) — preferred when present,
//      resolves the workspace path against the session cwd first;
//   2. engine `fs.open`/`fs.reveal` RPC — only when the host declares the
//      capability in /meta (browser connected to a capable engine);
//   3. neither (today's rust engine via plain browser) — the sentinel
//      ExternalOpenUnavailableError so the UI degrades to a copy-path prompt
//      instead of a raw failure.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { WsRpcApi } from '../src/api/daemon/ws-rpc-api';
import {
  ExternalOpenUnavailableError,
  isExternalOpenUnavailableError,
  type MirriDesktopBridge,
} from '../src/lib/desktopBridge';

type RpcCall = (method: string, params?: unknown) => Promise<unknown>;
type EventHandler = (event: unknown) => void;

const SESSION_VIEW = {
  id: 's1',
  title: 'S',
  createdAt: '2026-01-01T00:00:00Z',
  updatedAt: '2026-01-01T00:00:00Z',
  agentCount: 1,
  tags: [],
  agents: [{ id: 'main' }],
};

function createApi(): { api: WsRpcApi; call: ReturnType<typeof vi.fn<RpcCall>> } {
  const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
  const call = vi.fn<RpcCall>().mockResolvedValue(undefined);
  (api as unknown as { client: unknown }).client = {
    call,
    on: vi.fn<(type: string, handler: EventHandler) => () => void>(),
    onAnyEvent: vi.fn(() => () => {}),
    close: vi.fn(),
  };
  return { api, call };
}

function makeBridge(
  overrides: Partial<Record<keyof MirriDesktopBridge, unknown>> = {},
): MirriDesktopBridge {
  return {
    openPath: vi.fn().mockResolvedValue({ opened: true }),
    revealPath: vi.fn().mockResolvedValue({ revealed: true }),
    openInEditor: vi.fn().mockResolvedValue({ opened: true }),
    openInApp: vi.fn().mockResolvedValue({ opened: true }),
    openExternal: vi.fn().mockResolvedValue({ opened: true }),
    notify: vi.fn().mockResolvedValue({ shown: true }),
    setBadgeCount: vi.fn().mockResolvedValue({ set: true }),
    dockBounce: vi.fn().mockResolvedValue(undefined),
    setLoginItem: vi.fn().mockResolvedValue({ open: true }),
    getLoginItem: vi.fn().mockResolvedValue({ open: false }),
    onDeepLink: vi.fn(),
    onNotificationClick: vi.fn(),
    ...overrides,
  } as unknown as MirriDesktopBridge;
}

beforeEach(() => {
  // Session cwd for path resolution — every WsRpcApi test session lives at
  // /ws.
  vi.stubGlobal('window', {});
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('WsRpcApi.openFile degrade chain', () => {
  it('tier 1 — desktop bridge: opens the resolved absolute path in the editor', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'session.get') {
        return { ...SESSION_VIEW, cwd: '/ws' };
      }
      throw new Error(`unexpected call: ${method}`);
    });
    const bridge = makeBridge();
    (window as unknown as { mirriDesktop?: unknown }).mirriDesktop = bridge;

    await expect(api.openFile('s1', { path: 'src/a.rs', line: 7 })).resolves.toEqual({
      opened: true,
    });
    expect(bridge.openInEditor).toHaveBeenCalledWith('/ws/src/a.rs', 7);
    // The engine was only asked for the session cwd — no fs call.
    expect(call).toHaveBeenCalledTimes(1);
  });

  it('tier 1 — propagates a real bridge failure (missing file) instead of degrading', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'session.get') return { ...SESSION_VIEW, cwd: '/ws' };
      throw new Error(`unexpected call: ${method}`);
    });
    const bridge = makeBridge({
      openInEditor: vi.fn().mockRejectedValue(new Error('no such file')),
    });
    (window as unknown as { mirriDesktop?: unknown }).mirriDesktop = bridge;

    await expect(api.openFile('s1', { path: 'gone.rs' })).rejects.toThrow('no such file');
  });

  it('tier 2 — engine fs.open when the host declares the capability', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'fs.open') {
        return { opened: true };
      }
      throw new Error(`unexpected call: ${method}`);
    });
    // No bridge — plain browser. getMeta declares the capability via fetch.
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({ data: { version: 'x', capabilities: { 'fs.open': true } } }),
    });
    vi.stubGlobal('fetch', fetchMock);

    await expect(api.openFile('s1', { path: 'src/a.rs', line: 2 })).resolves.toEqual({
      opened: true,
    });
    expect(call).toHaveBeenCalledWith('fs.open', {
      session_id: 's1',
      path: 'src/a.rs',
      line: 2,
    });
    // The capability probe hits /meta exactly once (cached afterwards).
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it('tier 3 — plain browser against the rust engine: the sentinel error with the path', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'session.get') return { ...SESSION_VIEW, cwd: '/ws' };
      throw new Error(`unexpected call: ${method}`);
    });
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => ({ data: { version: 'x', capabilities: {} } }),
      }),
    );

    const attempt = api.openFile('s1', { path: 'src/a.rs' });
    await expect(attempt).rejects.toSatisfy((err: unknown) => isExternalOpenUnavailableError(err));
    try {
      await api.openFile('s1', { path: 'src/a.rs' });
    } catch (error) {
      expect((error as ExternalOpenUnavailableError).target).toBe('src/a.rs');
    }
  });

  it('tier 3 — a failing capability probe degrades rather than crashing', async () => {
    const { api } = createApi();
    vi.stubGlobal(
      'fetch',
      vi.fn().mockRejectedValue(new Error('fetch failed')),
    );
    await expect(api.openFile('s1', { path: 'a.rs' })).rejects.toSatisfy(
      (err: unknown) => isExternalOpenUnavailableError(err),
    );
  });
});

describe('WsRpcApi.revealFile degrade chain', () => {
  it('tier 1 — desktop bridge: reveals the resolved absolute path', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'session.get') return { ...SESSION_VIEW, cwd: '/ws' };
      throw new Error(`unexpected call: ${method}`);
    });
    const bridge = makeBridge();
    (window as unknown as { mirriDesktop?: unknown }).mirriDesktop = bridge;

    await expect(api.revealFile('s1', { path: 'out/main.cjs' })).resolves.toEqual({
      revealed: true,
    });
    expect(bridge.revealPath).toHaveBeenCalledWith('/ws/out/main.cjs');
  });

  it('tier 2 — engine fs.reveal when the capability is declared', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'fs.reveal') return { revealed: true };
      throw new Error(`unexpected call: ${method}`);
    });
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({ data: { version: 'x', capabilities: { 'fs.reveal': true } } }),
    }));

    await expect(api.revealFile('s1', { path: 'a.rs' })).resolves.toEqual({ revealed: true });
    expect(call).toHaveBeenCalledWith('fs.reveal', { session_id: 's1', path: 'a.rs' });
  });

  it('tier 3 — sentinel error in a plain browser', async () => {
    const { api } = createApi();
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({ data: { version: 'x', capabilities: {} } }),
    }));
    await expect(api.revealFile('s1', { path: 'a.rs' })).rejects.toSatisfy(
      (err: unknown) => isExternalOpenUnavailableError(err),
    );
  });
});

describe('WsRpcApi.openInApp degrade chain', () => {
  it('tier 1 — desktop bridge: opens the session cwd in the app', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'session.get') return { ...SESSION_VIEW, cwd: '/ws' };
      throw new Error(`unexpected call: ${method}`);
    });
    const bridge = makeBridge();
    (window as unknown as { mirriDesktop?: unknown }).mirriDesktop = bridge;

    await expect(api.openInApp('s1', 'vscode', '.')).resolves.toBeUndefined();
    expect(bridge.openInApp).toHaveBeenCalledWith('vscode', '/ws');
  });

  it('tier 3 — sentinel error in a plain browser', async () => {
    const { api } = createApi();
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({ data: { version: 'x', capabilities: {} } }),
    }));
    await expect(api.openInApp('s1', 'vscode', '.')).rejects.toSatisfy(
      (err: unknown) => isExternalOpenUnavailableError(err),
    );
  });
});
