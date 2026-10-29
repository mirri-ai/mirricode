// apps/mirri-web/test/ws-rpc-api-credentials.test.ts
// Task 4 of the credential closeout — the WsRpcApi credential-store
// adapters must speak the engine's wire contract: `credential.list` with
// the optional providerId scope, `credential.set` carrying the
// CredentialValue union verbatim, `credential.remove` /
// `credential.status` with the provider id, and the `credential.updated`
// subscription routed through the raw client channel (no session-id
// filtering) with the snake_case `provider_id` payload surfaced to the
// caller.

import { beforeEach, describe, expect, it, vi } from 'vitest';

import { WsRpcApi } from '../src/api/daemon/ws-rpc-api';
import type { RpcEvent } from '../src/api/daemon/json-rpc-client';

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

describe('WsRpcApi credential store adapters', () => {
  it('should call credential.list with no params when unscoped', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue([]);

    await api.credentialList();

    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith('credential.list', undefined);
  });

  it('should scope credential.list by providerId when given', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue([]);

    await api.credentialList('openai');

    expect(call).toHaveBeenCalledWith('credential.list', { providerId: 'openai' });
  });

  it('should map the snake_case list rows onto the app shape', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue([
      { provider_id: 'openai', type: 'key', time_created: 1770000000, status: 'normal' },
      {
        provider_id: 'lab',
        type: 'external',
        time_created: 1770000001,
        status: 'needs_reconfigure',
        external_available: false,
      },
    ]);

    const rows = await api.credentialList();

    expect(rows).toEqual([
      { providerId: 'openai', type: 'key', timeCreated: 1770000000, status: 'normal' },
      {
        providerId: 'lab',
        type: 'external',
        timeCreated: 1770000001,
        status: 'needs_reconfigure',
        externalAvailable: false,
      },
    ]);
  });

  it('should pass the credential value verbatim to credential.set', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue(null);

    await api.credentialSet('openai', { type: 'key', key: 'sk-test' });

    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith('credential.set', {
      providerId: 'openai',
      value: { type: 'key', key: 'sk-test' },
    });
  });

  it('should pass the external value verbatim to credential.set', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue(null);

    await api.credentialSet('lab', {
      type: 'external',
      method_id: 'azure-cli',
      metadata: { resource_name: 'https://management.core.windows.net/' },
    });

    expect(call).toHaveBeenCalledWith('credential.set', {
      providerId: 'lab',
      value: {
        type: 'external',
        method_id: 'azure-cli',
        metadata: { resource_name: 'https://management.core.windows.net/' },
      },
    });
  });

  it('should call credential.remove with the provider id', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue(null);

    await api.credentialRemove('openai');

    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith('credential.remove', { providerId: 'openai' });
  });

  it('should map the credential.status wire onto the app shape', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ status: 'needs_reconfigure', message: 'provider openai: 401 unauthorized' });

    const status = await api.credentialStatus('openai');

    expect(call).toHaveBeenCalledWith('credential.status', { providerId: 'openai' });
    expect(status).toEqual({ status: 'needs_reconfigure', message: 'provider openai: 401 unauthorized' });
  });
});

describe('WsRpcApi onCredentialUpdated', () => {
  /** Build a WsRpcApi whose transport is a mocked `client.on`. */
  function createApiWithOn(): {
    api: WsRpcApi;
    on: ReturnType<typeof vi.fn<(type: string, handler: (event: RpcEvent) => void) => () => void>>;
  } {
    const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
    const on = vi.fn<(type: string, handler: (event: RpcEvent) => void) => () => void>();
    (api as unknown as { client: { on: typeof on } }).client = { on };
    return { api, on };
  }

  it('should subscribe on the raw credential.updated channel and surface the snake_case provider_id', () => {
    const { api, on } = createApiWithOn();
    const seen: Array<string | undefined> = [];
    let registered: ((event: RpcEvent) => void) | undefined;
    const unsubscribe = vi.fn();
    on.mockImplementation((_type, handler) => {
      registered = handler;
      return unsubscribe;
    });

    const off = api.onCredentialUpdated((providerId) => {
      seen.push(providerId);
    });

    expect(on).toHaveBeenCalledTimes(1);
    expect(on).toHaveBeenCalledWith('credential.updated', expect.any(Function));

    registered!({ type: 'credential.updated', payload: { provider_id: 'openai' } });
    registered!({ type: 'credential.updated', payload: {} });

    expect(seen).toEqual(['openai', undefined]);

    off();
    expect(unsubscribe).toHaveBeenCalledTimes(1);
  });
});
