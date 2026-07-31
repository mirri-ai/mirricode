import { describe, expect, it, vi, afterEach, beforeEach } from 'vitest';

import {
  MIRRICODE_OAUTH_KEY,
  MIRRICODE_PLATFORM_ID,
  MIRRICODE_PROVIDER_NAME,
  type ManagedMirriConfigShape,
  type ManagedMirriOAuthRef,
} from '../src/managed-mirri-code';
import { refreshProviderModels, type RefreshProviderHost } from '../src/refreshProviderModels';

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

function aliasKey(modelId: string): string {
  return `${MIRRICODE_PLATFORM_ID}/${modelId}`;
}

function managedProvider(oauth?: ManagedMirriOAuthRef): Record<string, unknown> {
  return {
    type: 'openai',
    baseUrl: 'https://api.kimi.com/coding/v1',
    apiKey: '',
    oauth: oauth ?? { storage: 'file', key: MIRRICODE_OAUTH_KEY },
  };
}

function makeHost(initial: ManagedMirriConfigShape): RefreshProviderHost & {
  getConfig(): Promise<ManagedMirriConfigShape>;
} {
  let config: ManagedMirriConfigShape = structuredClone(initial);
  return {
    async getConfig() {
      return structuredClone(config);
    },
    async removeProvider(providerId: string) {
      const providers = { ...config.providers };
      delete providers[providerId];
      const models: Record<string, Record<string, unknown>> = {};
      for (const [key, raw] of Object.entries(config.models ?? {})) {
        if (typeof raw === 'object' && raw !== null && (raw as { provider?: string }).provider === providerId) continue;
        models[key] = raw as Record<string, unknown>;
      }
      config = { ...config, providers, models };
      return structuredClone(config);
    },
    async setConfig(patch) {
      // Deep-merge like the real daemon: nested records are merged key-by-key,
      // undefined values are dropped (cannot clear keys).
      const next: ManagedMirriConfigShape = structuredClone(config);
      const merged = deepMerge(next, patch as ManagedMirriConfigShape);
      config = merged;
      return structuredClone(config);
    },
    async resolveOAuthToken() {
      return 'oauth-access-token';
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
// Refresh semantics: catalog = template + new-model discovery.
//   - append only: existing alias fields are never modified
//   - never delete: models dropped from the remote list stay locally
//   - denylist: provider.removedModelIds blocks re-adding user-deleted models
// ---------------------------------------------------------------------------

describe('refreshProviderModels (managed Mirri Code)', () => {
  beforeEach(() => {
    vi.stubGlobal('fetch', vi.fn());
  });
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('appends new models without modifying existing alias fields', async () => {
    const initial: ManagedMirriConfigShape = {
      providers: { [MIRRICODE_PROVIDER_NAME]: managedProvider() },
      models: {
        [aliasKey('model-a')]: {
          provider: MIRRICODE_PROVIDER_NAME,
          model: 'model-a',
          maxContextSize: 100,
          capabilities: ['thinking'],
        },
      },
      defaultModel: aliasKey('model-a'),
      thinking: { enabled: true },
    };
    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        modelsResponse([
          { id: 'model-a', context_length: 200, supports_reasoning: true, display_name: 'Model A (renamed)' },
          { id: 'model-b', context_length: 300, supports_reasoning: true, display_name: 'Model B' },
        ]),
      ),
    );
    const host = makeHost(initial);

    await refreshProviderModels(host);

    const final = await host.getConfig();
    // Existing alias untouched — maxContextSize stays 100, capabilities stay as-is.
    expect(final.models?.[aliasKey('model-a')]).toMatchObject({
      provider: MIRRICODE_PROVIDER_NAME,
      model: 'model-a',
      maxContextSize: 100,
      capabilities: ['thinking'],
    });
    // New model appended.
    expect(final.models?.[aliasKey('model-b')]).toMatchObject({
      provider: MIRRICODE_PROVIDER_NAME,
      model: 'model-b',
    });
  });

  it('does not modify an existing alias even when remote fields differ', async () => {
    const initial: ManagedMirriConfigShape = {
      providers: { [MIRRICODE_PROVIDER_NAME]: managedProvider() },
      models: {
        [aliasKey('model-a')]: {
          provider: MIRRICODE_PROVIDER_NAME,
          model: 'model-a',
          maxContextSize: 100,
          capabilities: ['thinking'],
          displayName: 'My Custom Name',
        },
      },
      defaultModel: aliasKey('model-a'),
      thinking: { enabled: true },
    };
    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        modelsResponse([
          { id: 'model-a', context_length: 999, supports_reasoning: false, display_name: 'Remote Name' },
        ]),
      ),
    );
    const host = makeHost(initial);

    await refreshProviderModels(host);

    const final = await host.getConfig();
    expect(final.models?.[aliasKey('model-a')]).toMatchObject({
      maxContextSize: 100,
      displayName: 'My Custom Name',
      capabilities: ['thinking'],
    });
  });

  it('does not delete models that disappeared from the remote list', async () => {
    const initial: ManagedMirriConfigShape = {
      providers: { [MIRRICODE_PROVIDER_NAME]: managedProvider() },
      models: {
        [aliasKey('model-a')]: {
          provider: MIRRICODE_PROVIDER_NAME,
          model: 'model-a',
          maxContextSize: 100,
        },
        [aliasKey('model-b')]: {
          provider: MIRRICODE_PROVIDER_NAME,
          model: 'model-b',
          maxContextSize: 200,
        },
      },
      defaultModel: aliasKey('model-a'),
      thinking: { enabled: true },
    };
    // Remote only lists model-a; model-b is gone upstream.
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => modelsResponse([{ id: 'model-a', context_length: 100 }])),
    );
    const host = makeHost(initial);

    await refreshProviderModels(host);

    const final = await host.getConfig();
    expect(final.models?.[aliasKey('model-b')]).toMatchObject({
      provider: MIRRICODE_PROVIDER_NAME,
      model: 'model-b',
      maxContextSize: 200,
    });
  });

  it('does not re-append a model the user deleted (provider.removedModelIds denylist)', async () => {
    const initial: ManagedMirriConfigShape = {
      providers: {
        [MIRRICODE_PROVIDER_NAME]: {
          ...managedProvider(),
          removedModelIds: ['model-b'],
        },
      },
      models: {
        [aliasKey('model-a')]: {
          provider: MIRRICODE_PROVIDER_NAME,
          model: 'model-a',
          maxContextSize: 100,
        },
      },
      defaultModel: aliasKey('model-a'),
      thinking: { enabled: true },
    };
    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        modelsResponse([
          { id: 'model-a', context_length: 100 },
          { id: 'model-b', context_length: 200, display_name: 'Model B' },
        ]),
      ),
    );
    const host = makeHost(initial);

    await refreshProviderModels(host);

    const final = await host.getConfig();
    // model-b is NOT re-appended because it is on the denylist.
    expect(final.models?.[aliasKey('model-b')]).toBeUndefined();
    // model-a stays untouched.
    expect(final.models?.[aliasKey('model-a')]).toMatchObject({
      model: 'model-a',
      maxContextSize: 100,
    });
  });

  it('preserves a user-added alias that is not in the remote catalog', async () => {
    const initial: ManagedMirriConfigShape = {
      providers: { [MIRRICODE_PROVIDER_NAME]: managedProvider() },
      models: {
        [aliasKey('model-a')]: {
          provider: MIRRICODE_PROVIDER_NAME,
          model: 'model-a',
          maxContextSize: 100,
        },
        // User added this by hand; it is not a managed model id.
        'custom-alias': {
          provider: MIRRICODE_PROVIDER_NAME,
          model: 'my-custom-model',
          maxContextSize: 50000,
        },
      },
      defaultModel: aliasKey('model-a'),
      thinking: { enabled: true },
    };
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => modelsResponse([{ id: 'model-a', context_length: 100 }])),
    );
    const host = makeHost(initial);

    await refreshProviderModels(host);

    const final = await host.getConfig();
    expect(final.models?.['custom-alias']).toMatchObject({
      provider: MIRRICODE_PROVIDER_NAME,
      model: 'my-custom-model',
      maxContextSize: 50000,
    });
  });
});
