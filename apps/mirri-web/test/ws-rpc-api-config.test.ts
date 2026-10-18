// apps/mirri-web/test/ws-rpc-api-config.test.ts
// The WsRpcApi config adapters must forward patches to the daemon's
// `config.set` RPC with the retired `disabledAgents` key stripped at
// runtime — the type-level Omit is the first line of defense, this is the
// second for JS callers and stale bundles.

import { beforeEach, describe, expect, it, vi } from 'vitest';

import { WsRpcApi } from '../src/api/daemon/ws-rpc-api';

/** The JsonRpcClient.call signature the adapter under test relies on. */
type RpcCall = (method: string, params?: unknown) => Promise<unknown>;

/** Build a WsRpcApi whose transport is a mocked `client.call`. */
function createApi(): { api: WsRpcApi; call: ReturnType<typeof vi.fn<RpcCall>> } {
  const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
  const call = vi.fn<RpcCall>();
  (api as unknown as { client: { call: RpcCall } }).client = { call };
  return { api, call };
}

beforeEach(() => {
  vi.restoreAllMocks();
});

describe('WsRpcApi config', () => {
  it('should forward a legal patch to config.set unchanged', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({});

    await api.setConfig({ planMode: true });

    expect(call).toHaveBeenCalledWith('config.set', { planMode: true });
  });

  it('should strip the retired disabledAgents key before forwarding a patch to config.set', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({});

    // Smuggled in past the type (a stale caller still carrying the
    // retired key) — the adapter must drop it on the floor.
    const stalePatch = {
      planMode: true,
      disabledAgents: ['legacy-agent'],
    } as Parameters<WsRpcApi['setConfig']>[0];

    await api.setConfig(stalePatch);

    expect(call).toHaveBeenCalledWith('config.set', { planMode: true });
    expect(call).not.toHaveBeenCalledWith('config.set', expect.objectContaining({ disabledAgents: expect.anything() }));
  });
});
