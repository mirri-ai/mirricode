// apps/mirri-web/test/ws-rpc-api-models.test.ts
// Given the rust engine's batch model-management faces (B8: `model.delete`
// accepts `{id: string[]}`, `providerDiscovery.importCatalog` accepts
// `excludeModelIds`), the WsRpcApi adapters must forward the batch alias
// array as ONE rpc call (one write transaction on the engine, ADR-005) and
// carry the exclusion list through to the wire params verbatim.

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

describe('WsRpcApi model deletion and catalog import', () => {
  it('should send the alias array to model.delete as one call', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue(null);

    await api.deleteModel(['kimi/alpha', 'kimi/beta']);

    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith('model.delete', { id: ['kimi/alpha', 'kimi/beta'] });
  });

  it('should send a single-alias batch as an array to model.delete', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue(null);

    await api.deleteModel(['kimi/alpha']);

    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith('model.delete', { id: ['kimi/alpha'] });
  });

  it('should forward excludeModelIds to importCatalog', async () => {
    const { api, call } = createApi();
    // importCatalogProvider issues a second rpc (`model.getProvider`) via
    // fetchAppProvider to read the imported provider row back — the mock
    // answers the import first, then the read-back.
    const providerRow = {
      id: 'import-ai',
      type: 'openai',
      baseUrl: null,
      apiKey: null,
      defaultModel: 'import-ai/imp-large',
      models: [],
    };
    call.mockResolvedValueOnce({ provider: 'import-ai', models_imported: 2 });
    call.mockResolvedValueOnce(providerRow);

    const { provider, modelsImported } = await api.importCatalogProvider({
      catalogId: 'import-ai',
      excludeModelIds: ['imp-small'],
    });

    expect(call).toHaveBeenNthCalledWith(1, 'providerDiscovery.importCatalog', {
      catalogId: 'import-ai',
      id: undefined,
      apiKey: undefined,
      baseUrl: undefined,
      excludeModelIds: ['imp-small'],
    });
    expect(call).toHaveBeenNthCalledWith(2, 'model.getProvider', { providerId: 'import-ai' });
    expect(provider.id).toBe('import-ai');
    expect(modelsImported).toBe(2);
  });

  it('should omit excludeModelIds when the import carries none', async () => {
    const { api, call } = createApi();
    const providerRow = {
      id: 'import-ai',
      type: 'openai',
      baseUrl: null,
      apiKey: null,
      defaultModel: 'import-ai/imp-large',
      models: [],
    };
    call.mockResolvedValueOnce({ provider: 'import-ai', models_imported: 3 });
    call.mockResolvedValueOnce(providerRow);

    await api.importCatalogProvider({ catalogId: 'import-ai' });

    expect(call).toHaveBeenNthCalledWith(1, 'providerDiscovery.importCatalog', {
      catalogId: 'import-ai',
      id: undefined,
      apiKey: undefined,
      baseUrl: undefined,
      excludeModelIds: undefined,
    });
  });
});
