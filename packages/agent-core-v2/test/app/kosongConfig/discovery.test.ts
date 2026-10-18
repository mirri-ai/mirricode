/**
 * `app/kosongConfig` discovery tests — `IProviderDiscoveryService`:
 *
 *  - `refreshProviderModels` short-circuits `modelSource: 'static'`: a scoped
 *    refresh answers `unchanged` without any I/O, and an unscoped refresh
 *    hides the static entries from the orchestrator and merges them back
 *    verbatim — the static provider, its models, and a default model pointing
 *    at them all survive;
 *  - concurrent refreshes serialize (never overlap);
 *  - custom-registry fetches carry the host `User-Agent`;
 *  - provider/model patches land in config through ONE atomic
 *    `replaceSections` transition (the persistence bridge then syncs them
 *    into the registries), merging discovered aliases into user-owned
 *    provider records, while `defaultModel` / `thinking` ride the same
 *    transition — restoring a surviving default selection and CLEARing a
 *    default (plus its thinking) whose alias the upstream dropped, never a
 *    dangling `set`;
 *  - the two-phase orchestrator host contract (removeProvider, then
 *    setConfig) never exposes a halfway-removed catalog: the registries
 *    stay untouched until the single atomic write;
 *  - the `[modelCatalog]` config section self-registers and validates.
 */

import { afterEach, describe, expect, it, vi } from 'vitest';

import { createScopedTestHost } from '#/_base/di/test';
import { isError2 } from '#/_base/errors/errors';
import { ILogService, type LogPayload } from '#/_base/log/log';
import { IBootstrapService } from '#/app/bootstrap/bootstrap';
import { IConfigService } from '#/app/config/config';
import { ConfigRegistry } from '#/app/config/configService';
import { IEventService } from '#/app/event/event';
import { IProviderDiscoveryService } from '#/app/kosongConfig/discovery';
import '#/app/kosongConfig/discoveryService';
import { MODEL_CATALOG_SECTION } from '#/app/kosongConfig/configSection';
import { IKosongConfigService } from '#/app/kosongConfig/kosongConfig';
import '#/app/kosongConfig/kosongConfigService';
import '#/kosong/model/errors';
import {
  IModelService,
  type ModelRecord,
} from '#/kosong/model/model';
import '#/kosong/model/modelService';
import {
  IProviderService,
  type ProviderConfig,
} from '#/kosong/provider/provider';
import '#/kosong/provider/providerService';
import '#/kosong/provider/providers/kimi/kimi.contrib';
import '#/kosong/provider/providers/standard.contrib';

import { StubConfigService } from '../../kosong/stubs';
import { stubBootstrap } from '../bootstrap/stubs';

function stubEvents(): IEventService & { published: Array<{ type: string; payload: unknown }> } {
  const published: Array<{ type: string; payload: unknown }> = [];
  return {
    published,
    _serviceBrand: undefined,
    onDidPublish: () => ({ dispose: () => {} }),
    publish: (event: { type: string; payload: unknown }) => {
      published.push(event);
    },
    subscribe: () => ({ dispose: () => {} }),
  } as unknown as IEventService & { published: Array<{ type: string; payload: unknown }> };
}

function stubLogService(): ILogService {
  return {
    _serviceBrand: undefined,
    level: 'debug',
    setLevel: () => {},
    flush: async () => {},
    error: () => {},
    warn: () => {},
    info: () => {},
    debug: () => {},
    child: () => {
      throw new Error('child loggers are not used by KosongConfigService');
    },
  } satisfies ILogService;
}

async function createHost(
  sections: Record<string, unknown> = {},
): Promise<{
  host: ReturnType<typeof createScopedTestHost>;
  config: StubConfigService;
  events: ReturnType<typeof stubEvents>;
  discovery: IProviderDiscoveryService;
  providers: IProviderService;
  models: IModelService;
}> {
  const config = new StubConfigService(sections);
  const events = stubEvents();
  const host = createScopedTestHost([
    [IConfigService, config],
    [IEventService, events],
    [ILogService, stubLogService()],
    [
      IBootstrapService,
      stubBootstrap('/tmp/mirri-home', {}, { requestHeaders: { 'User-Agent': 'mirri-test/1.0' } }),
    ],
  ]);
  const providers = host.app.accessor.get(IProviderService);
  const models = host.app.accessor.get(IModelService);
  // The real persistence bridge (DI-activated): refresh writes land in
  // config, and the bridge's config → kosong sync is what carries them into
  // the registries (exactly like production). Await its initialization so
  // the event subscriptions are in place before the test refreshes.
  const bridge = host.app.accessor.get(IKosongConfigService);
  await bridge.ready;
  return {
    host,
    config,
    events,
    discovery: host.app.accessor.get(IProviderDiscoveryService),
    providers,
    models,
  };
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.unstubAllEnvs();
});

const staticProviders: Record<string, ProviderConfig> = {
  'static-p': { type: 'openai', modelSource: 'static', apiKey: 'sk-static' },
};

const staticModels: Record<string, ModelRecord> = {
  s1: { provider: 'static-p', model: 'static-model', maxContextSize: 1000 },
};

const staticSections: Record<string, unknown> = {
  providers: staticProviders,
  models: staticModels,
  defaultModel: 's1',
};

describe('refreshProviderModels modelSource short-circuit', () => {
  it('answers scoped refreshes of static providers with unchanged and no I/O', async () => {
    const fetchMock = vi.fn();
    vi.stubGlobal('fetch', fetchMock);
    const { host, discovery } = await createHost(staticSections);
    try {
      const result = await discovery.refreshProviderModels({ providerId: 'static-p' });
      expect(result).toEqual({ changed: [], unchanged: ['static-p'], failed: [] });
      expect(fetchMock).not.toHaveBeenCalled();
    } finally {
      host.dispose();
    }
  });

  it('returns an empty result when nothing is refreshable', async () => {
    const { host, discovery, events } = await createHost(staticSections);
    try {
      const result = await discovery.refreshProviderModels({ scope: 'all' });
      expect(result).toEqual({ changed: [], unchanged: [], failed: [] });
      expect(events.published).toEqual([]);
    } finally {
      host.dispose();
    }
  });

  it('hides static entries from the orchestrator and merges them back verbatim', async () => {
    const fetchMock = vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            acme: {
              id: 'acme',
              name: 'Acme',
              api: 'https://acme.example.test/v1',
              type: 'openai',
              models: { m1: { id: 'm1', name: 'M1' } },
            },
          }),
          { status: 200, headers: { 'Content-Type': 'application/json' } },
        ),
    );
    vi.stubGlobal('fetch', fetchMock);

    const { host, config, discovery, events, providers, models } = await createHost({
      providers: {
        ...staticProviders,
        acme: {
          type: 'openai',
          apiKey: 'sk-acme',
          source: { kind: 'apiJson', url: 'https://registry.example.test/api.json', apiKey: 'sk-registry' },
        },
      },
      models: staticModels,
      defaultModel: 's1',
      thinking: { enabled: true },
    });
    try {
      const result = await discovery.refreshProviderModels({ scope: 'all' });
      // The registry provider refreshed; the static one is nowhere in the result.
      expect(result.changed).toEqual([
        { provider_id: 'acme', provider_name: 'Acme', added: 1, removed: 0 },
      ]);
      expect(result.unchanged).toEqual([]);
      expect(result.failed).toEqual([]);
      expect(events.published).toEqual([
        expect.objectContaining({ type: 'event.model_catalog.changed' }),
      ]);

      // Static provider, its model, the default selection, and its thinking
      // all survived the orchestrator's whole-section writes. Providers and
      // models land in the in-memory registries (the persistence bridge owns
      // the config write-back); defaultModel/thinking go through config.
      const providerRecords = providers.list();
      expect(Object.keys(providerRecords).toSorted()).toEqual(['acme', 'static-p']);
      expect(providerRecords['static-p']).toEqual({ type: 'openai', modelSource: 'static', apiKey: 'sk-static' });
      const modelRecords = models.list();
      expect(modelRecords['s1']).toEqual({ provider: 'static-p', model: 'static-model', maxContextSize: 1000 });
      expect(modelRecords['acme/m1']).toBeDefined();
      expect(config.get<string>('defaultModel')).toBe('s1');
      expect(config.get('thinking')).toEqual({ enabled: true });
    } finally {
      host.dispose();
    }
  });

  it('throws provider.not_found for an unknown scoped provider', async () => {
    const { host, discovery } = await createHost(staticSections);
    try {
      await expect(discovery.refreshProviderModels({ providerId: 'missing' })).rejects.toSatisfy(
        (error) => isError2(error) && error.code === 'provider.not_found',
      );
    } finally {
      host.dispose();
    }
  });
});

describe('refreshProviderModels write behavior', () => {
  it('serializes concurrent runs so they never overlap', async () => {
    const { host, discovery } = await createHost({
      providers: {
        'moonshot-cn': {
          type: 'moonshot-cn',
          apiKey: 'sk-platform',
        },
      },
      models: {},
    });
    try {
      let inFlight = 0;
      let maxInFlight = 0;
      const fetchMock = vi.fn().mockImplementation(async () => {
        inFlight++;
        maxInFlight = Math.max(maxInFlight, inFlight);
        await new Promise((resolve) => setTimeout(resolve, 20));
        inFlight--;
        return new Response(
          JSON.stringify({
            data: [
              {
                id: 'kimi-k2',
                context_length: 131072,
                supports_reasoning: true,
                display_name: 'Kimi K2',
              },
            ],
          }),
          { status: 200, headers: { 'Content-Type': 'application/json' } },
        );
      });
      vi.stubGlobal('fetch', fetchMock);

      await Promise.all([
        discovery.refreshProviderModels({ scope: 'all' }),
        discovery.refreshProviderModels({ scope: 'all' }),
      ]);

      expect(maxInFlight).toBe(1);
      expect(fetchMock).toHaveBeenCalledTimes(2);
    } finally {
      host.dispose();
    }
  });

  it('sends the host User-Agent on custom-registry fetches', async () => {
    const fetchMock = vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            acme: {
              id: 'acme',
              name: 'Acme',
              api: 'https://acme.example.test/v1',
              type: 'openai',
              models: { m1: { id: 'm1', name: 'M1' } },
            },
          }),
          { headers: { 'Content-Type': 'application/json' } },
        ),
    );
    vi.stubGlobal('fetch', fetchMock);

    const { host, discovery } = await createHost({
      providers: {
        acme: {
          type: 'openai',
          apiKey: 'sk-acme',
          source: {
            kind: 'apiJson',
            url: 'https://registry.example.test/api.json',
            apiKey: 'sk-registry',
          },
        },
      },
      models: {},
    });
    try {
      await discovery.refreshProviderModels({ scope: 'all' });

      expect(fetchMock).toHaveBeenCalledWith(
        'https://registry.example.test/api.json',
        expect.objectContaining({
          headers: expect.objectContaining({ 'User-Agent': 'mirri-test/1.0' }),
        }),
      );
    } finally {
      host.dispose();
    }
  });

  it('refreshes an open-platform API-key provider model list', async () => {
    const fetchMock = vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            data: [
              {
                id: 'kimi-k2',
                context_length: 262144,
                supports_reasoning: true,
                display_name: 'Fresh K2',
              },
              { id: 'kimi-k2.5', context_length: 131072 },
            ],
          }),
          { status: 200, headers: { 'Content-Type': 'application/json' } },
        ),
    );
    vi.stubGlobal('fetch', fetchMock);

    const { host, config, discovery, events, providers, models } = await createHost({
      providers: {
        'moonshot-cn': { type: 'moonshot-cn', apiKey: 'sk-distributed-key' },
      },
      models: {
        'moonshot-cn/kimi-k2': {
          provider: 'moonshot-cn',
          model: 'kimi-k2',
          maxContextSize: 262144,
          displayName: 'Old K2',
        },
      },
      defaultModel: 'moonshot-cn/kimi-k2',
    });
    try {
      const result = await discovery.refreshProviderModels({ scope: 'all' });

      expect(result.failed).toEqual([]);
      expect(result.changed).toEqual([
        { provider_id: 'moonshot-cn', provider_name: 'Kimi Platform (API key · platform.kimi.com)', added: 1, removed: 0 },
      ]);
      expect(events.published).toEqual([
        expect.objectContaining({ type: 'event.model_catalog.changed' }),
      ]);
      expect(fetchMock).toHaveBeenCalledWith(
        'https://api.moonshot.cn/v1/models',
        expect.objectContaining({
          headers: expect.objectContaining({ Authorization: 'Bearer sk-distributed-key' }),
        }),
      );
      // The orchestrator rewrites the platform provider record (wire type,
      // platform base URL, persisted key); user aliases are left merged.
      expect(providers.list()['moonshot-cn']).toEqual({
        type: 'kimi',
        baseUrl: 'https://api.moonshot.cn/v1',
        apiKey: 'sk-distributed-key',
      });
      const modelRecords = models.list();
      expect(modelRecords['moonshot-cn/kimi-k2']?.displayName).toBe('Fresh K2');
      expect(modelRecords['moonshot-cn/kimi-k2.5']).toBeDefined();
      // The surviving default selection is written back, not cleared.
      expect(config.get<string>('defaultModel')).toBe('moonshot-cn/kimi-k2');
    } finally {
      host.dispose();
    }
  });

  it('re-points a stale defaultModel to the first upstream model when the selected alias dropped', async () => {
    const fetchMock = vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            data: [{ id: 'kimi-k3', context_length: 1048576, supports_reasoning: true }],
          }),
          { status: 200, headers: { 'Content-Type': 'application/json' } },
        ),
    );
    vi.stubGlobal('fetch', fetchMock);

    const { host, config, discovery, models } = await createHost({
      providers: {
        'moonshot-cn': { type: 'moonshot-cn', apiKey: 'sk-distributed-key' },
      },
      models: {
        'moonshot-cn/kimi-k2': {
          provider: 'moonshot-cn',
          model: 'kimi-k2',
          maxContextSize: 262144,
          displayName: 'Old K2',
        },
      },
      defaultModel: 'moonshot-cn/kimi-k2',
      thinking: { enabled: true },
    });
    try {
      const result = await discovery.refreshProviderModels({ scope: 'all' });

      expect(result.failed).toEqual([]);
      expect(result.changed).toEqual([
        { provider_id: 'moonshot-cn', provider_name: 'Kimi Platform (API key · platform.kimi.com)', added: 1, removed: 1 },
      ]);
      // The dropped alias was the default: the orchestrator re-points the
      // selection to the first upstream model instead of leaving it dangling.
      expect(config.get('defaultModel')).toBe('moonshot-cn/kimi-k3');
      expect(config.get('thinking')).toEqual({ enabled: false });
      const modelRecords = models.list();
      expect(modelRecords['moonshot-cn/kimi-k3']).toBeDefined();
      expect(modelRecords['moonshot-cn/kimi-k2']).toBeUndefined();
    } finally {
      host.dispose();
    }
  });

  it('never exposes a halfway-removed catalog: the registries stay untouched until the single atomic write', async () => {
    const fetchMock = vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            acme: {
              id: 'acme',
              name: 'Acme',
              api: 'https://acme.example.test/v1',
              type: 'openai',
              models: { m2: { id: 'm2', name: 'M2' } },
            },
          }),
          { headers: { 'Content-Type': 'application/json' } },
        ),
    );
    vi.stubGlobal('fetch', fetchMock);

    const { host, config, discovery, providers, models } = await createHost({
      providers: {
        acme: {
          type: 'openai',
          apiKey: 'sk-acme',
          source: {
            kind: 'apiJson',
            url: 'https://registry.example.test/api.json',
            apiKey: 'sk-registry',
          },
        },
      },
      models: {
        'acme/m1': { provider: 'acme', model: 'm1', maxContextSize: 1000 },
      },
      defaultModel: 'acme/m1',
    });
    try {
      // The orchestrator's host contract is two-phase (removeProvider, then
      // setConfig). By the time the atomic write happens, the removal phase
      // must NOT have touched the runtime registries — a reader (e.g. a
      // profile bind racing the refresh) only ever sees the old catalog or
      // the new one, never a half-removed one.
      let seenDuringWrite: { providers: readonly string[]; models: readonly string[] } | undefined;
      const originalReplaceSections = config.replaceSections.bind(config);
      vi.spyOn(config, 'replaceSections').mockImplementation(async (sections) => {
        seenDuringWrite = {
          providers: Object.keys(providers.list()),
          models: Object.keys(models.list()),
        };
        await originalReplaceSections(sections);
      });

      const result = await discovery.refreshProviderModels({ scope: 'all' });

      expect(result.failed).toEqual([]);
      expect(seenDuringWrite).toEqual({ providers: ['acme'], models: ['acme/m1'] });
      expect(vi.mocked(config.replaceSections).mock.calls.length).toBe(1);
      // After the write, config and the registries converge on the new state:
      // the dropped alias (and its default selection) is gone everywhere.
      expect(providers.list()['acme']).toBeDefined();
      expect(models.list()['acme/m2']).toBeDefined();
      expect(models.list()['acme/m1']).toBeUndefined();
      expect(config.get('defaultModel')).toBeUndefined();
    } finally {
      host.dispose();
    }
  });
});

describe('modelCatalog config section', () => {
  it('self-registers and validates', () => {
    const registry = new ConfigRegistry();
    expect(registry.getSection(MODEL_CATALOG_SECTION)).toBeDefined();
    expect(
      registry.validate(MODEL_CATALOG_SECTION, {
        refreshIntervalMs: 1000,
        refreshOnStart: false,
      }),
    ).toEqual({ refreshIntervalMs: 1000, refreshOnStart: false });
    expect(() => registry.validate(MODEL_CATALOG_SECTION, { refreshIntervalMs: -1 })).toThrow();
  });
});
