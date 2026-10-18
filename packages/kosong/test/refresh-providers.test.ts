import { afterEach, describe, expect, it, vi } from 'vitest';

import { refreshProviderModels } from '../src/refresh-providers';
import type { RefreshProviderHost } from '../src/refresh-providers';
import type { ProviderConfigShape } from '../src/provider-types';

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

interface RemoteModel {
  readonly id: string;
  readonly context_length?: number;
  readonly supports_reasoning?: boolean;
  readonly supports_image_in?: boolean;
  readonly supports_video_in?: boolean;
  readonly supports_tool_use?: boolean;
  readonly display_name?: string;
}

function modelsResponse(models: readonly RemoteModel[]): Response {
  return new Response(JSON.stringify({ data: models }), {
    status: 200,
    headers: { 'Content-Type': 'application/json' },
  });
}

function registryResponse(entries: Record<string, unknown>): Response {
  return new Response(JSON.stringify(entries), {
    status: 200,
    headers: { 'Content-Type': 'application/json' },
  });
}

function jsonErrorResponse(status: number, message: string): Response {
  return new Response(JSON.stringify({ error: { message } }), { status });
}

function openPlatformProvider(apiKey: string): Record<string, unknown> {
  return {
    type: 'kimi',
    baseUrl: 'https://api.moonshot.cn/v1',
    apiKey,
  };
}

function registrySource(url: string, apiKey: string): Record<string, unknown> {
  return {
    kind: 'apiJson',
    url,
    apiKey,
  };
}

function registryProvider(apiKey: string, source: Record<string, unknown>): Record<string, unknown> {
  return {
    type: 'openai',
    baseUrl: 'https://registry.example.test/v1',
    apiKey,
    source,
  };
}

function makeHost(initial: ProviderConfigShape): RefreshProviderHost {
  let config: ProviderConfigShape = structuredClone(initial);
  return {
    async getConfig() {
      return structuredClone(config);
    },
    async removeProvider(providerId: string) {
      const providers = { ...config.providers };
      delete providers[providerId];
      const models: Record<string, Record<string, unknown>> = {};
      for (const [key, raw] of Object.entries(config.models ?? {})) {
        if (
          typeof raw === 'object' &&
          raw !== null &&
          (raw as { provider?: string }).provider === providerId
        ) {
          // Mirror the real daemon: removing a provider also clears a
          // defaultModel that pointed at one of its aliases.
          if (config.defaultModel === key) config.defaultModel = undefined;
          continue;
        }
        models[key] = raw as Record<string, unknown>;
      }
      config = { ...config, providers, models };
      return structuredClone(config);
    },
    async setConfig(patch) {
      // Deep-merge like the real daemon: nested records are merged key-by-key,
      // undefined values are dropped (cannot clear keys).
      const next: ProviderConfigShape = structuredClone(config);
      config = deepMerge(next, patch as ProviderConfigShape);
      return structuredClone(config);
    },
  };
}

function deepMerge<T>(target: T, patch: unknown): T {
  if (typeof target !== 'object' || target === null || typeof patch !== 'object' || patch === null) {
    return (patch ?? target) as T;
  }
  const out: Record<string, unknown> = { ...(target as Record<string, unknown>) };
  for (const [key, value] of Object.entries(patch as Record<string, unknown>)) {
    if (value === undefined) continue;
    if (value !== null && typeof value === 'object' && !Array.isArray(value)) {
      out[key] = deepMerge(out[key] ?? {}, value);
    } else {
      out[key] = value;
    }
  }
  return out as T;
}

// ---------------------------------------------------------------------------
// Open-platform leg
// ---------------------------------------------------------------------------

describe('refreshProviderModels open-platform leg', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('should add platform models and report the provider as changed when the catalog lists models not in the config', async () => {
    const initial: ProviderConfigShape = {
      providers: { 'moonshot-cn': openPlatformProvider('sk-test') },
    };
    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        modelsResponse([
          { id: 'kimi-k2-0712-preview', context_length: 256000, supports_reasoning: true },
          { id: 'kimi-k2-lite', context_length: 128000, supports_reasoning: false },
        ]),
      ),
    );
    const host = makeHost(initial);

    const result = await refreshProviderModels(host);

    expect(result.changed).toEqual([
      {
        providerId: 'moonshot-cn',
        providerName: 'Kimi Platform (API key · platform.kimi.com)',
        added: 2,
        removed: 0,
      },
    ]);
    expect(result.unchanged).toEqual([]);
    expect(result.failed).toEqual([]);

    const final = await host.getConfig();
    expect(final.models?.['moonshot-cn/kimi-k2-0712-preview']).toMatchObject({
      provider: 'moonshot-cn',
      model: 'kimi-k2-0712-preview',
      maxContextSize: 256000,
    });
    expect(final.models?.['moonshot-cn/kimi-k2-lite']).toMatchObject({
      provider: 'moonshot-cn',
      model: 'kimi-k2-lite',
      maxContextSize: 128000,
    });
  });

  it('should report the open-platform provider as unchanged when the catalog and config models match', async () => {
    const initial: ProviderConfigShape = {
      providers: { 'moonshot-cn': openPlatformProvider('sk-test') },
      models: {
        'moonshot-cn/kimi-k2-0712-preview': {
          provider: 'moonshot-cn',
          model: 'kimi-k2-0712-preview',
          maxContextSize: 256000,
          capabilities: ['thinking', 'image_in', 'video_in', 'tool_use'],
          displayName: 'Kimi K2 0712 Preview',
        },
      },
      defaultModel: 'moonshot-cn/kimi-k2-0712-preview',
    };
    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        modelsResponse([
          {
            id: 'kimi-k2-0712-preview',
            context_length: 256000,
            supports_reasoning: true,
            supports_image_in: true,
            supports_video_in: true,
            display_name: 'Kimi K2 0712 Preview',
          },
        ]),
      ),
    );
    const host = makeHost(initial);

    const result = await refreshProviderModels(host);

    expect(result.changed).toEqual([]);
    expect(result.unchanged).toEqual(['moonshot-cn']);
    expect(result.failed).toEqual([]);
  });

  it('should only refresh the targeted open-platform provider when providerId is passed', async () => {
    const initial: ProviderConfigShape = {
      providers: {
        'moonshot-cn': openPlatformProvider('sk-test'),
        'moonshot-ai': { ...openPlatformProvider('sk-test'), baseUrl: 'https://api.moonshot.ai/v1' },
      },
    };
    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        modelsResponse([{ id: 'kimi-k2-0712-preview', context_length: 256000, supports_reasoning: true }]),
      ),
    );
    const host = makeHost(initial);

    const result = await refreshProviderModels(host, { providerId: 'moonshot-cn' });

    expect(result.changed.map((c) => c.providerId)).toEqual(['moonshot-cn']);

    const final = await host.getConfig();
    expect(final.models?.['moonshot-cn/kimi-k2-0712-preview']).toBeDefined();
    // The sibling platform must not have been refreshed.
    expect(final.models?.['moonshot-ai/kimi-k2-0712-preview']).toBeUndefined();
  });

  it('should record the failure and keep the previous config when the platform fetch returns an error', async () => {
    const initial: ProviderConfigShape = {
      providers: { 'moonshot-cn': openPlatformProvider('sk-bad') },
    };
    vi.stubGlobal('fetch', vi.fn(async () => jsonErrorResponse(401, 'invalid API key')));
    const host = makeHost(initial);

    const result = await refreshProviderModels(host);

    expect(result.changed).toEqual([]);
    expect(result.failed).toEqual([
      { provider: 'moonshot-cn', reason: 'invalid API key' },
    ]);

    const final = await host.getConfig();
    expect(final.models).toBeUndefined();
  });

  it('should skip an open-platform provider when it has no apiKey configured', async () => {
    const initial: ProviderConfigShape = {
      providers: { 'moonshot-cn': { type: 'kimi', baseUrl: 'https://api.moonshot.cn/v1' } },
    };
    const fetchMock = vi.fn(async () => modelsResponse([]));
    vi.stubGlobal('fetch', fetchMock);
    const host = makeHost(initial);

    const result = await refreshProviderModels(host);

    expect(result.changed).toEqual([]);
    expect(result.unchanged).toEqual([]);
    expect(result.failed).toEqual([]);
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('should preserve the existing default model when it is still listed by the platform', async () => {
    const initial: ProviderConfigShape = {
      providers: { 'moonshot-cn': openPlatformProvider('sk-test') },
      models: {
        'moonshot-cn/kimi-k2-0712-preview': {
          provider: 'moonshot-cn',
          model: 'kimi-k2-0712-preview',
          maxContextSize: 256000,
        },
      },
      defaultModel: 'moonshot-cn/kimi-k2-0712-preview',
      thinking: { enabled: true },
    };
    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        modelsResponse([
          {
            id: 'kimi-k2-0712-preview',
            context_length: 256000,
            supports_reasoning: true,
            display_name: 'Kimi K2 0712 Preview',
          },
          { id: 'kimi-k2-lite', context_length: 128000, supports_reasoning: true },
        ]),
      ),
    );
    const host = makeHost(initial);

    await refreshProviderModels(host);

    const final = await host.getConfig();
    expect(final.defaultModel).toBe('moonshot-cn/kimi-k2-0712-preview');
  });

  it('should preserve user-curated aliases when they are not part of the platform catalog', async () => {
    const initial: ProviderConfigShape = {
      providers: { 'moonshot-cn': openPlatformProvider('sk-test') },
      models: {
        // Keyed outside the `moonshot-cn/` prefix, so the refresh does not
        // treat it as a generated alias.
        'handpicked-k2': {
          provider: 'moonshot-cn',
          model: 'kimi-k2-0712-preview',
          maxContextSize: 999,
          displayName: 'My Custom Entry',
        },
      },
      defaultModel: 'handpicked-k2',
    };
    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        modelsResponse([{ id: 'kimi-k2-0712-preview', context_length: 256000, supports_reasoning: true }]),
      ),
    );
    const host = makeHost(initial);

    await refreshProviderModels(host);

    const final = await host.getConfig();
    expect(final.models?.['handpicked-k2']).toMatchObject({
      provider: 'moonshot-cn',
      model: 'kimi-k2-0712-preview',
      maxContextSize: 999,
      displayName: 'My Custom Entry',
    });
    expect(final.models?.['moonshot-cn/kimi-k2-0712-preview']).toBeDefined();
    expect(final.defaultModel).toBe('handpicked-k2');
  });
});

// ---------------------------------------------------------------------------
// Custom-registry leg
// ---------------------------------------------------------------------------

describe('refreshProviderModels custom-registry leg', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  const REGISTRY_URL = 'https://registry.example.test/v1/models/api.json';

  it('should add registry models and report the provider as changed when the registry gains models', async () => {
    const initial: ProviderConfigShape = {
      providers: {
        'registry-chat': registryProvider('sk-token', registrySource(REGISTRY_URL, 'sk-token')),
      },
    };
    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        registryResponse({
          'registry-chat': {
            id: 'registry-chat',
            name: 'Registry Chat',
            api: 'https://registry.example.test/v1',
            type: 'openai',
            models: { 'gpt-5.5': { id: 'gpt-5.5', name: 'GPT 5.5' } },
          },
        }),
      ),
    );
    const host = makeHost(initial);

    const result = await refreshProviderModels(host);

    expect(result.changed).toEqual([
      { providerId: 'registry-chat', providerName: 'Registry Chat', added: 1, removed: 0 },
    ]);

    const final = await host.getConfig();
    expect(final.models?.['registry-chat/gpt-5.5']).toMatchObject({
      provider: 'registry-chat',
      model: 'gpt-5.5',
      maxContextSize: 131072,
      displayName: 'GPT 5.5',
    });
  });

  it('should report the custom-registry provider as unchanged when models and provider record match the registry', async () => {
    const initial: ProviderConfigShape = {
      providers: {
        'registry-chat': {
          type: 'openai',
          baseUrl: 'https://registry.example.test/v1',
          apiKey: 'sk-token',
          source: registrySource(REGISTRY_URL, 'sk-token'),
        },
      },
      models: {
        'registry-chat/gpt-5.5': {
          provider: 'registry-chat',
          model: 'gpt-5.5',
          maxContextSize: 131072,
          capabilities: ['tool_use'],
          displayName: 'GPT 5.5',
        },
      },
    };
    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        registryResponse({
          'registry-chat': {
            id: 'registry-chat',
            name: 'Registry Chat',
            api: 'https://registry.example.test/v1',
            type: 'openai',
            models: { 'gpt-5.5': { id: 'gpt-5.5', name: 'GPT 5.5' } },
          },
        }),
      ),
    );
    const host = makeHost(initial);

    const result = await refreshProviderModels(host);

    expect(result.changed).toEqual([]);
    expect(result.unchanged).toEqual(['registry-chat']);
    expect(result.failed).toEqual([]);
  });

  it('should rotate to the next stored API key when the first registry credential is rejected', async () => {
    const initial: ProviderConfigShape = {
      providers: {
        'registry-a': registryProvider('sk-old', registrySource(REGISTRY_URL, 'sk-old')),
        'registry-b': registryProvider('sk-new', registrySource(REGISTRY_URL, 'sk-new')),
      },
    };
    const fetchMock = vi
      .fn()
      .mockImplementationOnce(async () => jsonErrorResponse(401, 'invalid bearer'))
      .mockImplementationOnce(async () =>
        registryResponse({
          'registry-a': {
            id: 'registry-a',
            name: 'Registry A',
            api: 'https://registry.example.test/v1',
            type: 'openai',
            models: { 'm-1': { id: 'm-1' } },
          },
          'registry-b': {
            id: 'registry-b',
            name: 'Registry B',
            api: 'https://registry.example.test/v1',
            type: 'openai',
            models: { 'm-1': { id: 'm-1' } },
          },
        }),
      );
    vi.stubGlobal('fetch', fetchMock);
    const host = makeHost(initial);

    const result = await refreshProviderModels(host);

    expect(fetchMock).toHaveBeenCalledTimes(2);
    const calls = fetchMock.mock.calls as unknown as Array<[string, RequestInit]>;
    expect(
      (calls[0]?.[1]?.headers as Record<string, string>)['Authorization'],
    ).toBe('Bearer sk-old');
    expect(
      (calls[1]?.[1]?.headers as Record<string, string>)['Authorization'],
    ).toBe('Bearer sk-new');

    expect(result.failed).toEqual([]);
    // Both providers picked up the winning credential: no failure, both applied.
    const final = await host.getConfig();
    expect(final.providers['registry-a']?.['apiKey']).toBe('sk-new');
    expect(final.providers['registry-b']?.['apiKey']).toBe('sk-new');
    expect(final.models?.['registry-a/m-1']).toBeDefined();
    expect(final.models?.['registry-b/m-1']).toBeDefined();
  });

  it('should remove a custom-registry provider and clear its default when the provider disappears from the registry', async () => {
    const initial: ProviderConfigShape = {
      providers: {
        'registry-chat': registryProvider('sk-token', registrySource(REGISTRY_URL, 'sk-token')),
      },
      models: {
        'registry-chat/gpt-5.5': {
          provider: 'registry-chat',
          model: 'gpt-5.5',
          maxContextSize: 131072,
        },
      },
      defaultModel: 'registry-chat/gpt-5.5',
    };
    vi.stubGlobal('fetch', vi.fn(async () => registryResponse({})));
    const host = makeHost(initial);

    const result = await refreshProviderModels(host);

    expect(result.changed).toEqual([
      { providerId: 'registry-chat', providerName: 'registry-chat', added: 0, removed: 1 },
    ]);

    const final = await host.getConfig();
    expect(final.providers['registry-chat']).toBeUndefined();
    expect(final.models?.['registry-chat/gpt-5.5']).toBeUndefined();
    expect(final.defaultModel).toBeUndefined();
  });

  it('should refresh only the targeted provider when providerId selects one member of a registry group', async () => {
    const initial: ProviderConfigShape = {
      providers: {
        'registry-a': registryProvider('sk-token', registrySource(REGISTRY_URL, 'sk-token')),
        'registry-b': registryProvider('sk-token', registrySource(REGISTRY_URL, 'sk-token')),
      },
    };
    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        registryResponse({
          'registry-a': {
            id: 'registry-a',
            name: 'Registry A',
            api: 'https://registry.example.test/v1',
            type: 'openai',
            models: { 'm-1': { id: 'm-1' } },
          },
          'registry-b': {
            id: 'registry-b',
            name: 'Registry B',
            api: 'https://registry.example.test/v1',
            type: 'openai',
            models: { 'm-1': { id: 'm-1' } },
          },
        }),
      ),
    );
    const host = makeHost(initial);

    const result = await refreshProviderModels(host, { providerId: 'registry-a' });

    expect(result.changed.map((c) => c.providerId)).toEqual(['registry-a']);

    const final = await host.getConfig();
    expect(final.models?.['registry-a/m-1']).toBeDefined();
    // A scoped refresh must not add sibling providers from the same registry.
    expect(final.models?.['registry-b/m-1']).toBeUndefined();
  });

  it('should report every provider in the group as failed when the registry fetch errors', async () => {
    const initial: ProviderConfigShape = {
      providers: {
        'registry-a': registryProvider('sk-token', registrySource(REGISTRY_URL, 'sk-token')),
        'registry-b': registryProvider('sk-token', registrySource(REGISTRY_URL, 'sk-token')),
      },
    };
    vi.stubGlobal('fetch', vi.fn(async () => jsonErrorResponse(500, 'registry down')));
    const host = makeHost(initial);

    const result = await refreshProviderModels(host);

    expect(result.changed).toEqual([]);
    expect(result.failed).toEqual([
      { provider: 'registry-a', reason: 'registry down' },
      { provider: 'registry-b', reason: 'registry down' },
    ]);
  });

  it('should preserve user-curated aliases when refreshing a custom-registry provider', async () => {
    const initial: ProviderConfigShape = {
      providers: {
        'registry-chat': registryProvider('sk-token', registrySource(REGISTRY_URL, 'sk-token')),
      },
      models: {
        // Keyed outside the `registry-chat/` prefix, so the refresh does not
        // treat it as a generated alias.
        'handpicked-gpt': {
          provider: 'registry-chat',
          model: 'gpt-5.5',
          maxContextSize: 999,
          displayName: 'My Custom Entry',
        },
      },
      defaultModel: 'handpicked-gpt',
    };
    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        registryResponse({
          'registry-chat': {
            id: 'registry-chat',
            name: 'Registry Chat',
            api: 'https://registry.example.test/v1',
            type: 'openai',
            models: { 'gpt-5.5': { id: 'gpt-5.5', name: 'GPT 5.5' } },
          },
        }),
      ),
    );
    const host = makeHost(initial);

    await refreshProviderModels(host);

    const final = await host.getConfig();
    expect(final.models?.['handpicked-gpt']).toMatchObject({
      provider: 'registry-chat',
      model: 'gpt-5.5',
      maxContextSize: 999,
      displayName: 'My Custom Entry',
    });
    expect(final.models?.['registry-chat/gpt-5.5']).toBeDefined();
    expect(final.defaultModel).toBe('handpicked-gpt');
  });
});