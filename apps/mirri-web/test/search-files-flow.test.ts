// apps/mirri-web/test/search-files-flow.test.ts
// E9 Task 2 — the searchFiles data flow upgrade: the workspace state layer
// must pass the engine's structured fields (kind / score / matchPositions /
// root / indexState / truncated) through to the mention menu instead of
// collapsing items to {path, name}, and must search the workspace root when
// no session is active yet (the draft-state arm). The transport adapters must
// expose file.stat (v3) and an explicit v3-only rejection (v2).
import { computed } from 'vue';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { AppWorkspace } from '../src/api/types';
import { useWorkspaceState, type UseWorkspaceStateDeps } from '../src/composables/client/useWorkspaceState';
import type { ExtendedState } from '../src/composables/useMirriWebClient';
import { createInitialState } from '../src/api/daemon/eventReducer';
import { i18n } from '../src/i18n';
import { WsRpcApi } from '../src/api/daemon/ws-rpc-api';
import { DaemonMirriWebApi } from '../src/api/daemon/client';

const apiMock = vi.hoisted(() => ({
  searchFiles: vi.fn(),
  connectTransport: vi.fn().mockResolvedValue(undefined),
}));

vi.mock('../src/api', () => ({
  getMirriWebApi: () => apiMock,
}));

const confirmMock = vi.hoisted(() => vi.fn());
vi.mock('../src/composables/useConfirmDialog', () => ({
  useConfirmDialog: () => ({ confirm: confirmMock }),
}));

/** The JsonRpcClient.call signature the adapter under test relies on. */
type RpcCall = (method: string, params?: unknown) => Promise<unknown>;

/** Build a WsRpcApi whose transport is a mocked `client.call`. */
function createWsApi(): { api: WsRpcApi; call: ReturnType<typeof vi.fn<RpcCall>> } {
  const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
  const call = vi.fn<RpcCall>();
  (api as unknown as { client: { call: RpcCall } }).client = { call };
  return { api, call };
}

function workspace(id: string, root: string): AppWorkspace {
  return {
    id,
    root,
    name: id,
    isGitRepo: false,
    sessionCount: 0,
    trusted: true,
  };
}

function createState(overrides: Partial<ExtendedState> = {}): ExtendedState {
  return {
    ...createInitialState(),
    sessions: [],
    activeSessionId: null,
    workspaces: [],
    activeWorkspaceId: null,
    connected: true,
    ...overrides,
  } as ExtendedState;
}

function createDeps(workspaces: AppWorkspace[] = []): UseWorkspaceStateDeps {
  return {
    mergedWorkspaces: computed(() => workspaces),
    workspacesView: computed(() =>
      workspaces.map((w) => ({ ...w, shortPath: w.root })),
    ),
    workspaceIdForSession: (s: { cwd: string }) => s.cwd,
    pushOperationFailure: vi.fn(),
  } as unknown as UseWorkspaceStateDeps;
}

function engineResult(overrides: Record<string, unknown> = {}) {
  return {
    items: [
      {
        path: 'src/components',
        name: 'components',
        kind: 'directory',
        score: 0.8,
        matchPositions: [0, 1, 2],
        root: 'mirricode',
      },
      {
        path: 'src/a.ts',
        name: 'a.ts',
        kind: 'file',
        score: 0.5,
        matchPositions: [4],
        root: 'mirricode',
      },
    ],
    truncated: false,
    indexState: 'ready',
    ...overrides,
  };
}

beforeEach(() => {
  apiMock.searchFiles.mockReset();
});

describe('useWorkspaceState — searchFiles passthrough (E9)', () => {
  it('Given the engine returns structured items, When searchFiles is called on an active session, Then kind/score/matchPositions/root/indexState/truncated all pass through', async () => {
    apiMock.searchFiles.mockResolvedValue(engineResult({ indexState: 'building' }));
    const state = createState({ activeSessionId: 'sess_1' });
    const ws = useWorkspaceState(state, createDeps());

    const result = await ws.searchFiles('comp');

    expect(apiMock.searchFiles).toHaveBeenCalledWith('sess_1', { query: 'comp', limit: 20 });
    expect(result.items).toHaveLength(2);
    expect(result.items[0]).toMatchObject({
      path: 'src/components',
      name: 'components',
      kind: 'directory',
      score: 0.8,
      matchPositions: [0, 1, 2],
      root: 'mirricode',
    });
    expect(result.indexState).toBe('building');
    expect(result.truncated).toBe(false);
  });

  it('Given no session is active but a workspace is, When searchFiles is called, Then the engine gets the workspace root instead of a session id', async () => {
    apiMock.searchFiles.mockResolvedValue(engineResult());
    const ws1 = workspace('ws_1', '/repo/root');
    const state = createState({
      activeSessionId: null,
      workspaces: [ws1],
      activeWorkspaceId: 'ws_1',
    });
    const ws = useWorkspaceState(state, createDeps([ws1]));

    const result = await ws.searchFiles('a');

    expect(apiMock.searchFiles).toHaveBeenCalledWith(null, {
      query: 'a',
      limit: 20,
      workspaceRoot: '/repo/root',
    });
    expect(result.items[0]).toMatchObject({ kind: 'directory' });
  });

  it('Given no session and no workspace root is resolvable, When searchFiles is called, Then the answer is an empty ready result without a backend call', async () => {
    const state = createState({ activeSessionId: null, workspaces: [], activeWorkspaceId: null });
    const ws = useWorkspaceState(state, createDeps());

    const result = await ws.searchFiles('a');

    expect(apiMock.searchFiles).not.toHaveBeenCalled();
    expect(result).toMatchObject({ items: [], truncated: false, indexState: 'ready' });
  });

  it('Given the transport rejects the search, When searchFiles is called, Then the answer is an empty ready result instead of a throw', async () => {
    apiMock.searchFiles.mockRejectedValue(new Error('boom'));
    const state = createState({ activeSessionId: 'sess_1' });
    const ws = useWorkspaceState(state, createDeps());

    const result = await ws.searchFiles('a');

    expect(result).toMatchObject({ items: [], truncated: false, indexState: 'ready' });
  });
});

describe('WsRpcApi — file.search / file.stat (E9)', () => {
  it('Given an older engine omits indexState and root, When the search result is mapped, Then indexState defaults to ready and root to an empty string', async () => {
    const { api, call } = createWsApi();
    call.mockResolvedValue({
      items: [{ path: 'a.ts', name: 'a.ts', kind: 'file', score: 1, matchPositions: [] }],
      truncated: true,
    });

    const result = await api.searchFiles('s-1', { query: 'a' });

    expect(call).toHaveBeenCalledWith('file.search', { sessionId: 's-1', query: 'a' });
    expect(result.indexState).toBe('ready');
    expect(result.items[0]?.root).toBe('');
    expect(result.truncated).toBe(true);
  });

  it('Given the engine answers file.stat, When statFiles is called, Then the items pass through with mtimeMs mapped from the wire', async () => {
    const { api, call } = createWsApi();
    call.mockResolvedValue({
      items: [
        { path: 'src/a.ts', exists: true, kind: 'file', readable: true, mtimeMs: 1234 },
        { path: 'gone.ts', exists: false, kind: 'file', readable: false, mtimeMs: null },
      ],
    });

    const result = await api.statFiles('s-1', ['src/a.ts', 'gone.ts']);

    expect(call).toHaveBeenCalledWith('file.stat', { sessionId: 's-1', paths: ['src/a.ts', 'gone.ts'] });
    expect(result.items[0]).toMatchObject({ path: 'src/a.ts', exists: true, mtimeMs: 1234 });
    expect(result.items[1]).toMatchObject({ exists: false, mtimeMs: null });
  });
});

describe('DaemonMirriWebApi (v2) — statFiles (E9)', () => {
  it('Given the v2 transport has no file.stat route, When statFiles is called, Then it rejects with the v3-only marker error', async () => {
    const client = new DaemonMirriWebApi({
      serverHttpUrl: 'http://127.0.0.1:1',
      clientId: 'test',
      clientName: 'test',
      clientVersion: '0.0.0',
      clientUiMode: 'test',
    });
    await expect(client.statFiles('s-1', ['a.ts'])).rejects.toThrow(/v3/);
  });
});

// The i18n import keeps the singleton initialized the same way the app does;
// useWorkspaceState reads t() at construction time.
void i18n;
