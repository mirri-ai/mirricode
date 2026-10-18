// apps/mirri-web/test/ws-rpc-workspace-control.test.ts
// Given the W6 workspace-control wire faces on the rust engine —
// `workspace.update` {workspaceId, name} → the renamed WorkspaceView, and
// the three-choice `workspace.delete` {workspaceId, mode} →
// {ok, mode, workspaceId, deletedSessions} (mode always sent explicitly,
// never defaulted into the destructive path client-side) — the WsRpcApi
// must send exactly those calls and map the camelCase results back.

import { beforeEach, describe, expect, it, vi } from 'vitest';

import { WsRpcApi } from '../src/api/daemon/ws-rpc-api';

/** The JsonRpcClient.call signature the adapters under test rely on. */
type RpcCall = (method: string, params?: unknown) => Promise<unknown>;
type EventHandler = (event: unknown) => void;

function createApi(): {
  api: WsRpcApi;
  call: ReturnType<typeof vi.fn<RpcCall>>;
} {
  const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
  const call = vi.fn<RpcCall>().mockResolvedValue(undefined);
  (api as unknown as { client: unknown }).client = {
    call,
    connect: vi.fn<() => Promise<void>>().mockResolvedValue(undefined),
    on: vi.fn<(type: string, handler: EventHandler) => () => void>(),
    onAnyEvent: vi.fn(() => () => {}),
    close: vi.fn(),
  };
  return { api, call };
}

beforeEach(() => {
  vi.restoreAllMocks();
});

describe('WsRpcApi deleteWorkspace', () => {
  it('should send the purge mode explicitly and map the deletion result', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      ok: true,
      mode: 'purge',
      workspaceId: 'wd_a',
      deletedSessions: 3,
    });

    const result = await api.deleteWorkspace('wd_a', 'purge');

    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith('workspace.delete', {
      workspaceId: 'wd_a',
      mode: 'purge',
    });
    expect(result).toEqual({
      ok: true,
      mode: 'purge',
      workspaceId: 'wd_a',
      deletedSessions: 3,
    });
  });

  it('should send the remove mode explicitly and report zero deleted sessions', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      ok: true,
      mode: 'remove',
      workspaceId: 'wd_a',
      deletedSessions: 0,
    });

    const result = await api.deleteWorkspace('wd_a', 'remove');

    expect(call).toHaveBeenCalledWith('workspace.delete', {
      workspaceId: 'wd_a',
      mode: 'remove',
    });
    expect(result.deletedSessions).toBe(0);
  });

  it('should default to the data-preserving remove mode when none is given', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      ok: true,
      mode: 'remove',
      workspaceId: 'wd_a',
      deletedSessions: 0,
    });

    await api.deleteWorkspace('wd_a');

    expect(call).toHaveBeenCalledWith('workspace.delete', {
      workspaceId: 'wd_a',
      mode: 'remove',
    });
  });

  it('should propagate the engine error when the default workspace is refused', async () => {
    const { api, call } = createApi();
    const error = Object.assign(new Error('cannot delete the default workspace wd_a'), {
      name: 'JsonRpcCallError',
      code: -32602,
    });
    call.mockRejectedValue(error);

    await expect(api.deleteWorkspace('wd_a', 'purge')).rejects.toThrow(
      'cannot delete the default workspace',
    );
  });
});

describe('WsRpcApi updateWorkspace', () => {
  it('should send the rename and map the renamed WorkspaceView', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      id: 'wd_a',
      root: '/abs/project',
      name: 'renamed',
      createdAt: '2026-09-11T00:00:00Z',
      lastOpenedAt: '2026-09-11T00:00:00Z',
      sessionCount: 0,
    });

    const workspace = await api.updateWorkspace('wd_a', { name: 'renamed' });

    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith('workspace.update', {
      workspaceId: 'wd_a',
      name: 'renamed',
    });
    expect(workspace.id).toBe('wd_a');
    expect(workspace.name).toBe('renamed');
    expect(workspace.root).toBe('/abs/project');
  });
});

// W8: the workspace trust management wire face — `workspace.setTrust`
// {workspaceId, trusted} → {trusted} (the TS v2 trust face's shape), and
// the workspace rows gain the live trust verdict.
describe('WsRpcApi setWorkspaceTrust', () => {
  it('should send the grant and answer with the engine trust result', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ trusted: true });

    const result = await api.setWorkspaceTrust('wd_untrusted', true);

    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith('workspace.setTrust', {
      workspaceId: 'wd_untrusted',
      trusted: true,
    });
    expect(result).toEqual({ trusted: true });
  });

  it('should send the revoke and answer with the engine trust result', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ trusted: false });

    const result = await api.setWorkspaceTrust('wd_trusted', false);

    expect(call).toHaveBeenCalledWith('workspace.setTrust', {
      workspaceId: 'wd_trusted',
      trusted: false,
    });
    expect(result).toEqual({ trusted: false });
  });

  it('should propagate the engine refusal when untrusting the pinned default', async () => {
    const { api, call } = createApi();
    call.mockRejectedValue(
      new (class extends Error {
        code = -32602;
      })('cannot untrust the default workspace'),
    );

    await expect(api.setWorkspaceTrust('wd_default', false)).rejects.toThrow(
      /cannot untrust the default workspace/,
    );
  });

  it('should map the trusted field onto the workspace rows', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue([
      {
        id: 'wd_default',
        root: '/abs/default',
        name: 'default',
        createdAt: '2026-09-11T00:00:00Z',
        lastOpenedAt: '2026-09-11T00:00:00Z',
        sessionCount: 1,
        trusted: true,
      },
      {
        id: 'wd_new',
        root: '/abs/new',
        name: 'new',
        createdAt: '2026-09-11T00:00:00Z',
        lastOpenedAt: '2026-09-11T00:00:00Z',
        sessionCount: 0,
        trusted: false,
      },
    ]);

    const workspaces = await api.listWorkspaces();

    expect(workspaces[0]?.trusted).toBe(true);
    expect(workspaces[1]?.trusted).toBe(false);
  });
});
