import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, describe, expect, it, vi } from 'vitest';

import type {
  CoreRPC,
  GetMirriConfigPayload,
  MirriConfig,
  MirriConfigPatch,
  SetMirriConfigPayload,
} from '../../src';

import {
  type ICoreProcessService,
  type IEnvironmentService,
  ModelCatalogService,
  ModelNotFoundError,
  ProviderNotFoundError,
  toProtocolModel,
  toProtocolProvider,
} from '../../src/services';
import type { IEventService } from '../../src/services/event/event';
import type { Event as ProtocolEvent } from '@mirri-ai/protocol';

afterEach(() => {
  vi.unstubAllGlobals();
  vi.clearAllMocks();
});

function makeEnv(): IEnvironmentService {
  return {
    _serviceBrand: undefined,
    homeDir: '/tmp/mirri-model-catalog-test',
    configPath: '/tmp/mirri-model-catalog-test/config.toml',
  };
}

function makeCore(configRef: { current: MirriConfig }): {
  core: ICoreProcessService;
  getCalls: GetMirriConfigPayload[];
  setCalls: MirriConfigPatch[];
  removeCalls: string[];
} {
  const getCalls: GetMirriConfigPayload[] = [];
  const setCalls: MirriConfigPatch[] = [];
  const removeCalls: string[] = [];
  const rpc: Partial<CoreRPC> = {
    getMirriConfig: vi.fn(async (payload: GetMirriConfigPayload) => {
      getCalls.push(payload);
      return configRef.current;
    }),
    setMirriConfig: vi.fn(async (payload: SetMirriConfigPayload) => {
      setCalls.push(payload);
      const next: MirriConfig = { ...configRef.current };
      if (payload.providers !== undefined) {
        next.providers = payload.providers as MirriConfig['providers'];
      }
      if (payload.models !== undefined) {
        next.models = payload.models as MirriConfig['models'];
      }
      if (payload.defaultModel !== undefined) next.defaultModel = payload.defaultModel;
      if (payload.thinking !== undefined) next.thinking = payload.thinking;
      configRef.current = next;
      return configRef.current;
    }),
    removeMirriProvider: vi.fn(async ({ providerId }) => {
      removeCalls.push(providerId);
      const providers = { ...configRef.current.providers };
      delete providers[providerId];
      const models = Object.fromEntries(
        Object.entries(configRef.current.models ?? {}).filter(([, model]) => model.provider !== providerId),
      ) as MirriConfig['models'];
      configRef.current = {
        ...configRef.current,
        providers,
        models,
        defaultModel: undefined,
      };
      return configRef.current;
    }),
  };
  return {
    core: {
      _serviceBrand: undefined,
      rpc: rpc as CoreRPC,
      ready: async () => undefined,
      dispose: () => undefined,
    },
    getCalls,
    setCalls,
    removeCalls,
  };
}

function makeEventService(): { svc: IEventService; published: ProtocolEvent[] } {
  const published: ProtocolEvent[] = [];
  const svc: IEventService = {
    _serviceBrand: undefined,
    onDidPublish: () => ({ dispose: () => undefined }),
    publish: (event) => {
      published.push(event);
    },
  };
  return { svc, published };
}

function catalogConfig(): MirriConfig {
  return {
    providers: {
      kimi: {
        type: 'openai',
        apiKey: 'sk-test',
        baseUrl: 'https://api.example.test/v1',
      },
      openai: { type: 'openai' },
    },
    defaultModel: 'k2',
    models: {
      k2: {
        provider: 'kimi',
        model: 'kimi-k2',
        maxContextSize: 131072,
        displayName: 'Kimi K2',
        capabilities: ['thinking'],
      },
      turbo: {
        provider: 'kimi',
        model: 'kimi-turbo',
        maxContextSize: 32768,
      },
      gpt4o: {
        provider: 'openai',
        model: 'gpt-4o',
        maxContextSize: 128000,
      },
    },
  };
}

// Custom-registry (models.dev-style api.json) fixture: a single provider whose
// remote entry exactly mirrors the already-provisioned config, so an unscoped
// refresh is a no-op for the baseline case.
function registryConfig(): MirriConfig {
  return {
    providers: {
      'registry-test': {
        // Key order matches what `applyCustomRegistryProvider` writes
        // (type, baseUrl, apiKey, source) so the provider-config snapshot is
        // byte-identical and an unchanged refresh is a true no-op.
        type: 'openai',
        baseUrl: 'https://registry.example.test/v1',
        apiKey: 'reg-key',
        source: { kind: 'apiJson', url: 'https://registry.example.test/api.json', apiKey: 'reg-key' },
      },
    },
    defaultModel: 'registry-test/base-model',
    models: {
      'registry-test/base-model': {
        provider: 'registry-test',
        model: 'base-model',
        maxContextSize: 128000,
        capabilities: ['tool_use'],
        displayName: 'Base Model',
      },
    },
  };
}

function registryResponse(entries: Record<string, unknown>): Response {
  return new Response(JSON.stringify(entries), {
    status: 200,
    headers: { 'Content-Type': 'application/json' },
  });
}

function registryEntry(models: Record<string, unknown>): Record<string, unknown> {
  return {
    id: 'registry-test',
    name: 'Registry Test',
    api: 'https://registry.example.test/v1',
    type: 'openai',
    models,
  };
}

function registryModel(id: string, name: string, context: number): Record<string, unknown> {
  return {
    id,
    name,
    limit: { context },
  };
}

describe('model catalog adapters', () => {
  it('maps model aliases to selectable wire ids', () => {
    const alias = catalogConfig().models!['k2']!;
    expect(toProtocolModel('k2', alias)).toEqual({
      provider: 'kimi',
      model: 'k2',
      display_name: 'Kimi K2',
      max_context_size: 131072,
      capabilities: ['thinking'],
    });
  });

  it('uses the provider model name as display fallback', () => {
    const alias = catalogConfig().models!['turbo']!;
    expect(toProtocolModel('turbo', alias).display_name).toBe('kimi-turbo');
  });

  it('maps provider model ids and global default', () => {
    const config = catalogConfig();
    expect(
      toProtocolProvider('kimi', config.providers['kimi']!, config, {
        hasApiKey: true,
        hasOAuthToken: false,
      }),
    ).toEqual({
      id: 'kimi',
      type: 'openai',
      base_url: 'https://api.example.test/v1',
      default_model: 'k2',
      has_api_key: true,
      status: 'connected',
      models: ['k2', 'turbo'],
    });
  });
});

describe('ModelCatalogService', () => {
  it('lists models and providers from live config', async () => {
    const configRef = { current: catalogConfig() };
    const { core, getCalls } = makeCore(configRef);
    const svc = new ModelCatalogService(makeEnv(), core, makeEventService().svc);

    expect(await svc.listModels()).toHaveLength(3);
    expect(await svc.listProviders()).toHaveLength(2);
    expect(getCalls).toEqual([{ reload: true }, { reload: true }]);
  });

  it('gets one provider or throws ProviderNotFoundError', async () => {
    const configRef = { current: catalogConfig() };
    const { core } = makeCore(configRef);
    const svc = new ModelCatalogService(makeEnv(), core, makeEventService().svc);

    await expect(svc.getProvider('kimi')).resolves.toMatchObject({ id: 'kimi' });
    await expect(svc.getProvider('missing')).rejects.toBeInstanceOf(
      ProviderNotFoundError,
    );
  });

  it('sets defaultModel through core config patch', async () => {
    const configRef = { current: catalogConfig() };
    const { core, setCalls } = makeCore(configRef);
    const svc = new ModelCatalogService(makeEnv(), core, makeEventService().svc);

    await expect(svc.setDefaultModel('turbo')).resolves.toEqual({
      default_model: 'turbo',
      model: {
        provider: 'kimi',
        model: 'turbo',
        display_name: 'kimi-turbo',
        max_context_size: 32768,
      },
    });
    expect(setCalls).toEqual([{ defaultModel: 'turbo' }]);
  });

  it('rejects unknown model ids', async () => {
    const configRef = { current: catalogConfig() };
    const { core } = makeCore(configRef);
    const svc = new ModelCatalogService(makeEnv(), core, makeEventService().svc);

    await expect(svc.setDefaultModel('missing')).rejects.toBeInstanceOf(
      ModelNotFoundError,
    );
  });

  it('given an unchanged custom registry, when refresh runs, then config is left untouched and no event is published', async () => {
    const configRef = { current: registryConfig() };
    const { core, setCalls, removeCalls } = makeCore(configRef);
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          registryResponse({
            'registry-test': registryEntry({
              'base-model': registryModel('base-model', 'Base Model', 128000),
            }),
          }),
      ),
    );
    const { svc: eventService, published } = makeEventService();
    const svc = new ModelCatalogService(makeEnv(), core, eventService);

    const result = await svc.refreshProviderModels();

    expect(result.changed).toEqual([]);
    expect(result.unchanged).toEqual(['registry-test']);
    expect(result.failed).toEqual([]);
    // No provider removal or config write happens when nothing changed.
    expect(removeCalls).toEqual([]);
    expect(setCalls).toEqual([]);
    expect(published).toEqual([]);
  });

  it('given a custom registry with a new model, when refresh runs, then the alias is appended and a changed event is published', async () => {
    const configRef = { current: registryConfig() };
    const { core, setCalls } = makeCore(configRef);
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          registryResponse({
            'registry-test': registryEntry({
              'base-model': registryModel('base-model', 'Base Model', 128000),
              'new-model': registryModel('new-model', 'New Model', 256000),
            }),
          }),
      ),
    );
    const { svc: eventService, published } = makeEventService();
    const svc = new ModelCatalogService(makeEnv(), core, eventService);

    const result = await svc.refreshProviderModels();

    expect(result.changed).toEqual([
      { provider_id: 'registry-test', provider_name: 'Registry Test', added: 1, removed: 0 },
    ]);
    expect(result.unchanged).toEqual([]);
    // The append lands on disk via setConfig and is reflected in the config.
    const last = setCalls.at(-1) as Record<string, unknown> | undefined;
    const models = last?.['models'] as Record<string, { provider: string; model: string }> | undefined;
    expect(models?.['registry-test/new-model']).toMatchObject({
      provider: 'registry-test',
      model: 'new-model',
      maxContextSize: 256000,
      displayName: 'New Model',
    });
    // Existing alias is preserved.
    expect(models?.['registry-test/base-model']).toMatchObject({
      provider: 'registry-test',
      model: 'base-model',
    });
    expect(published).toHaveLength(1);
    expect(published[0]!.type).toBe('event.model_catalog.changed');
  });

  it('given a scoped refresh for an unknown provider, when refresh runs, then ProviderNotFoundError is thrown', async () => {
    const configRef = { current: catalogConfig() };
    const { core } = makeCore(configRef);
    const svc = new ModelCatalogService(makeEnv(), core, makeEventService().svc);

    await expect(svc.refreshProviderModels({ providerId: 'missing' })).rejects.toBeInstanceOf(
      ProviderNotFoundError,
    );
  });

  it('given an oauth provider whose token is cached under the key stem, when providers are listed, then the provider is connected', async () => {
    const homeDir = mkdtempSync(join(tmpdir(), 'mirri-model-catalog-oauth-'));
    try {
      mkdirSync(join(homeDir, 'credentials'), { recursive: true });
      writeFileSync(
        join(homeDir, 'credentials', 'mirri-code.json'),
        JSON.stringify({ access_token: 'tok' }),
        'utf8',
      );

      const configRef: { current: MirriConfig } = {
        current: {
          providers: {
            // Provider id deliberately differs from the token storage name.
            'managed:mirri-code': {
              type: 'openai',
              apiKey: '',
              oauth: { storage: 'file', key: 'oauth/mirri-code' },
            },
          },
        },
      };
      const { core } = makeCore(configRef);
      const svc = new ModelCatalogService(
        { _serviceBrand: undefined, homeDir, configPath: join(homeDir, 'config.toml') },
        core,
        makeEventService().svc,
      );

      const [provider] = await svc.listProviders();
      expect(provider).toMatchObject({
        id: 'managed:mirri-code',
        has_api_key: false,
        status: 'connected',
      });
    } finally {
      rmSync(homeDir, { recursive: true, force: true });
    }
  });

  it('given an oauth provider with no cached token, when providers are listed, then the provider is unconfigured', async () => {
    const homeDir = mkdtempSync(join(tmpdir(), 'mirri-model-catalog-oauth-'));
    try {
      const configRef: { current: MirriConfig } = {
        current: {
          providers: {
            'managed:mirri-code': {
              type: 'openai',
              apiKey: '',
              oauth: { storage: 'file', key: 'oauth/mirri-code' },
            },
          },
        },
      };
      const { core } = makeCore(configRef);
      const svc = new ModelCatalogService(
        { _serviceBrand: undefined, homeDir, configPath: join(homeDir, 'config.toml') },
        core,
        makeEventService().svc,
      );

      const [provider] = await svc.listProviders();
      expect(provider).toMatchObject({
        id: 'managed:mirri-code',
        has_api_key: false,
        status: 'unconfigured',
      });
    } finally {
      rmSync(homeDir, { recursive: true, force: true });
    }
  });
});