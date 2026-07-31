import { describe, expect, it, vi } from 'vitest';

import {
  applyMirriManagedCodeLogoutConfig,
  applyManagedMirriCodeConfig,
  clearManagedMirriCodeConfig,
  fetchManagedMirriCodeModels,
  MIRRICODE_OAUTH_KEY,
  MIRRICODE_PROVIDER_NAME,
  ManagedMirriCodeModelsAuthError,
  provisionManagedMirriCodeConfig,
  resolveMirriCodeLoginAuth,
  resolveMirriCodeOAuthKey,
  resolveMirriCodeOAuthRef,
  resolveMirriCodeRuntimeAuth,
  type ManagedMirriCodeModelInfo,
  type ManagedMirriConfigShape,
} from '../src/managed-mirri-code';
import { OAuthUnauthorizedError } from '../src/errors';

function makeModelsResponse(): Response {
  return new Response(
    JSON.stringify({
      data: [
        {
          id: 'kimi-for-coding',
          context_length: 262144,
          supports_reasoning: true,
          supports_image_in: true,
          supports_video_in: true,
          display_name: 'Kimi for Coding',
        },
        {
          id: 'kimi-k2.5',
          context_length: 250000,
          supports_reasoning: false,
          supports_image_in: false,
          supports_video_in: false,
          supports_tool_use: false,
        },
      ],
    }),
    { status: 200, headers: { 'Content-Type': 'application/json' } },
  );
}

describe('provisionManagedMirriCodeConfig', () => {
  it('keeps the legacy credential key for the default production environment', () => {
    expect(
      resolveMirriCodeOAuthKey({
        oauthHost: 'https://auth.kimi.com/',
        baseUrl: 'https://api.kimi.com/coding/v1/',
      }),
    ).toBe(MIRRICODE_OAUTH_KEY);
  });

  it('scopes credential keys for non-default OAuth hosts and API base URLs', () => {
    const devKey = resolveMirriCodeOAuthKey({
      oauthHost: 'https://auth.dev.example.test',
      baseUrl: 'https://api.dev.example.test/coding/v1',
    });

    expect(devKey).not.toBe(MIRRICODE_OAUTH_KEY);
    expect(devKey).toMatch(/^oauth\/mirri-code-env-[a-f0-9]{16}$/);
    expect(
      resolveMirriCodeOAuthKey({
        oauthHost: 'https://auth.dev.example.test/',
        baseUrl: 'https://api.dev.example.test/coding/v1/',
      }),
    ).toBe(devKey);
  });

  it('derives a full OAuth ref whose key and persisted host stay in sync', () => {
    // Default environment collapses to the legacy ref (no persisted host), so
    // existing production credentials keep resolving to `mirri-code.json`.
    expect(
      resolveMirriCodeOAuthRef({
        oauthHost: 'https://auth.kimi.com/',
        baseUrl: 'https://api.kimi.com/coding/v1/',
      }),
    ).toEqual({ storage: 'file', key: MIRRICODE_OAUTH_KEY, oauthHost: undefined });

    const defaultAuthCustomApiRef = resolveMirriCodeOAuthRef({
      baseUrl: 'https://api.example.test/coding/v1',
    });
    expect(defaultAuthCustomApiRef).toEqual({
      storage: 'file',
      key: resolveMirriCodeOAuthKey({
        oauthHost: 'https://auth.kimi.com',
        baseUrl: 'https://api.example.test/coding/v1',
      }),
      oauthHost: 'https://auth.kimi.com',
    });

    // A non-default environment yields a scoped key AND the normalized host,
    // both derived from the same input — login and runtime cannot drift apart.
    const devRef = resolveMirriCodeOAuthRef({
      oauthHost: 'https://auth.dev.example.test/',
      baseUrl: 'https://api.dev.example.test/coding/v1',
    });
    expect(devRef).toEqual({
      storage: 'file',
      key: resolveMirriCodeOAuthKey({
        oauthHost: 'https://auth.dev.example.test',
        baseUrl: 'https://api.dev.example.test/coding/v1',
      }),
      oauthHost: 'https://auth.dev.example.test',
    });
  });

  it('resolves runtime auth from environment overrides over persisted config', () => {
    const configuredBaseUrl = 'https://api.configured.example.test/coding/v1';
    const envBaseUrl = 'https://api.env.example.test/coding/v1/';
    const envOauthHost = 'https://auth.env.example.test/';
    const configuredOAuthRef = resolveMirriCodeOAuthRef({
      baseUrl: configuredBaseUrl,
    });

    const auth = resolveMirriCodeRuntimeAuth({
      configuredBaseUrl,
      configuredOAuthRef,
      env: {
        MIRRICODE_BASE_URL: envBaseUrl,
        MIRRICODE_OAUTH_HOST: envOauthHost,
      },
    });

    expect(auth.baseUrl).toBe('https://api.env.example.test/coding/v1');
    expect(auth.oauthRef).toEqual({
      storage: 'file',
      key: resolveMirriCodeOAuthKey({
        oauthHost: 'https://auth.env.example.test',
        baseUrl: 'https://api.env.example.test/coding/v1',
      }),
      oauthHost: 'https://auth.env.example.test',
    });
  });

  it('preserves a matching configured runtime OAuth ref when env is not overridden', () => {
    const baseUrl = 'https://api.dev.example.test/coding/v1';
    const configuredOAuthRef = {
      storage: 'keyring' as const,
      key: resolveMirriCodeOAuthKey({
        oauthHost: 'https://auth.dev.example.test',
        baseUrl,
      }),
      oauthHost: 'https://auth.dev.example.test',
    };

    expect(
      resolveMirriCodeRuntimeAuth({
        configuredBaseUrl: baseUrl,
        configuredOAuthRef,
        env: {},
      }),
    ).toEqual({
      baseUrl,
      oauthRef: configuredOAuthRef,
    });
  });

  it('resolves login auth without reusing persisted refs under explicit or env overrides', () => {
    const configuredBaseUrl = 'https://api.configured.example.test/coding/v1';
    const configuredOAuthRef = resolveMirriCodeOAuthRef({ baseUrl: configuredBaseUrl });

    expect(
      resolveMirriCodeLoginAuth({
        configuredBaseUrl,
        configuredOAuthRef,
        requestedBaseUrl: 'https://api.requested.example.test/coding/v1/',
        env: {},
      }),
    ).toEqual({
      baseUrl: 'https://api.requested.example.test/coding/v1',
      oauthHost: undefined,
    });

    expect(
      resolveMirriCodeLoginAuth({
        configuredBaseUrl,
        configuredOAuthRef,
        env: {},
      }),
    ).toEqual({
      baseUrl: configuredBaseUrl,
      oauthHost: undefined,
      oauthRef: configuredOAuthRef,
    });
  });

  it('writes the managed provider, models, services, and default model through an adapter', async () => {
    const config: ManagedMirriConfigShape = {
      providers: {
        custom: {
          type: 'openai',
          apiKey: 'sk-existing',
          baseUrl: 'https://example.test/v1',
        },
      },
      models: {
        'mirri-code/stale': {
          provider: MIRRICODE_PROVIDER_NAME,
          model: 'stale',
        },
        'custom-default': {
          provider: 'custom',
          model: 'custom-model',
        },
      },
    };
    const write = vi.fn();
    const fetchMock = vi.fn(async () => makeModelsResponse());

    const result = await provisionManagedMirriCodeConfig({
      accessToken: 'oauth-access-token',
      fetchImpl: fetchMock as unknown as typeof fetch,
      adapter: {
        configPath: '/tmp/config.toml',
        read: () => config,
        write,
        apply: applyManagedMirriCodeConfig,
      },
    });

    expect(result).toMatchObject({
      providerName: MIRRICODE_PROVIDER_NAME,
      defaultModel: 'mirri-code/kimi-for-coding',
      defaultThinking: true,
      configPath: '/tmp/config.toml',
    });
    expect(result.models[0]?.supportsToolUse).toBe(true);
    expect(result.models[1]?.supportsToolUse).toBe(false);
    expect(fetchMock).toHaveBeenCalledWith(
      'https://api.kimi.com/coding/v1/models',
      expect.objectContaining({
        headers: expect.objectContaining({
          Authorization: 'Bearer oauth-access-token',
          Accept: 'application/json',
        }),
      }),
    );
    const calls = fetchMock.mock.calls as unknown as [string, RequestInit?][];
    const init = calls[0]?.[1] ?? {};
    const headers = new Headers((init.headers ?? {}) as Record<string, string>);
    expect(headers.get('user-agent')).toBeNull();
    expect(headers.get('x-msh-platform')).toBeNull();
    expect(write).toHaveBeenCalledWith(config);

    expect(config.providers['custom']).toMatchObject({
      apiKey: 'sk-existing',
    });
    expect(config.models?.['custom-default']?.provider).toBe('custom');
    // Existing managed alias is preserved (append-only refresh).
    expect(config.models?.['mirri-code/stale']).toMatchObject({
      provider: MIRRICODE_PROVIDER_NAME,
      model: 'stale',
    });
    expect(config.providers[MIRRICODE_PROVIDER_NAME]).toMatchObject({
      type: 'openai',
      baseUrl: 'https://api.kimi.com/coding/v1',
      apiKey: '',
      oauth: { storage: 'file', key: 'oauth/mirri-code' },
    });
    expect(config.models?.['mirri-code/kimi-for-coding']).toMatchObject({
      provider: MIRRICODE_PROVIDER_NAME,
      model: 'kimi-for-coding',
      maxContextSize: 262144,
      capabilities: ['thinking', 'image_in', 'video_in', 'tool_use'],
      displayName: 'Kimi for Coding',
    });
    expect(config.models?.['mirri-code/kimi-k2.5']?.capabilities).toBeUndefined();
    expect(config.services?.moonshotSearch).toMatchObject({
      baseUrl: 'https://api.kimi.com/coding/v1/search',
      apiKey: '',
      oauth: { storage: 'file', key: 'oauth/mirri-code' },
    });
    expect(Object.keys(config.services ?? {})).toEqual(['moonshotSearch', 'moonshotFetch']);
  });

  it('writes scoped OAuth refs when provisioning against a non-default environment', async () => {
    const config: ManagedMirriConfigShape = {
      providers: {},
    };
    const oauthKey = resolveMirriCodeOAuthKey({
      oauthHost: 'https://auth.dev.example.test',
      baseUrl: 'https://api.dev.example.test/coding/v1',
    });

    await provisionManagedMirriCodeConfig({
      accessToken: 'oauth-access-token',
      baseUrl: 'https://api.dev.example.test/coding/v1',
      oauthKey,
      oauthHost: 'https://auth.dev.example.test',
      fetchImpl: vi.fn(async () => makeModelsResponse()) as unknown as typeof fetch,
      adapter: {
        read: () => config,
        write: vi.fn(),
        apply: applyManagedMirriCodeConfig,
      },
    });

    expect(config.providers[MIRRICODE_PROVIDER_NAME]).toMatchObject({
      baseUrl: 'https://api.dev.example.test/coding/v1',
      oauth: {
        storage: 'file',
        key: oauthKey,
        oauthHost: 'https://auth.dev.example.test',
      },
    });
    expect(config.services?.moonshotSearch?.oauth).toEqual({
      storage: 'file',
      key: oauthKey,
      oauthHost: 'https://auth.dev.example.test',
    });
    expect(config.services?.moonshotFetch?.oauth).toEqual({
      storage: 'file',
      key: oauthKey,
      oauthHost: 'https://auth.dev.example.test',
    });
  });

  it('persists the default OAuth host when only the API base URL is scoped', async () => {
    const config: ManagedMirriConfigShape = {
      providers: {},
    };
    const baseUrl = 'https://api.example.test/coding/v1';
    const oauthKey = resolveMirriCodeOAuthKey({ baseUrl });

    await provisionManagedMirriCodeConfig({
      accessToken: 'oauth-access-token',
      baseUrl,
      fetchImpl: vi.fn(async () => makeModelsResponse()) as unknown as typeof fetch,
      adapter: {
        read: () => config,
        write: vi.fn(),
        apply: applyManagedMirriCodeConfig,
      },
    });

    expect(config.providers[MIRRICODE_PROVIDER_NAME]).toMatchObject({
      baseUrl,
      oauth: {
        storage: 'file',
        key: oauthKey,
        oauthHost: 'https://auth.kimi.com',
      },
    });
  });

  it('preserves an existing valid default model during refresh', async () => {
    const config: ManagedMirriConfigShape = {
      providers: {
        custom: {
          type: 'openai',
          apiKey: 'sk-existing',
          baseUrl: 'https://example.test/v1',
        },
        [MIRRICODE_PROVIDER_NAME]: {
          type: 'openai',
          apiKey: '',
        },
      },
      defaultModel: 'custom-default',
      thinking: { enabled: false },
      models: {
        'custom-default': {
          provider: 'custom',
          model: 'custom-model',
          maxContextSize: 1000,
        },
        'mirri-code/stale': {
          provider: MIRRICODE_PROVIDER_NAME,
          model: 'stale',
          maxContextSize: 1000,
        },
      },
    };

    const result = await provisionManagedMirriCodeConfig({
      accessToken: 'oauth-access-token',
      fetchImpl: vi.fn(async () => makeModelsResponse()) as unknown as typeof fetch,
      preserveDefaultModel: true,
      adapter: {
        read: () => config,
        write: vi.fn(),
        apply: applyManagedMirriCodeConfig,
      },
    });

    expect(result.defaultModel).toBe('custom-default');
    expect(result.defaultThinking).toBe(false);
    expect(config.defaultModel).toBe('custom-default');
    expect(config.thinking?.enabled).toBe(false);
    // Existing managed alias preserved (append-only refresh).
    expect(config.models?.['mirri-code/stale']).toMatchObject({
      provider: MIRRICODE_PROVIDER_NAME,
      model: 'stale',
    });
    expect(config.models?.['mirri-code/kimi-for-coding']?.displayName).toBe('Kimi for Coding');
  });

  it('infers default_thinking from fresh managed model capabilities', async () => {
    const config: ManagedMirriConfigShape = {
      providers: {
        [MIRRICODE_PROVIDER_NAME]: {
          type: 'openai',
          apiKey: '',
        },
      },
      defaultModel: 'mirri-code/kimi-for-coding',
      models: {
        'mirri-code/kimi-for-coding': {
          provider: MIRRICODE_PROVIDER_NAME,
          model: 'kimi-for-coding',
          maxContextSize: 1000,
          capabilities: [],
        },
      },
    };

    const result = await provisionManagedMirriCodeConfig({
      accessToken: 'oauth-access-token',
      fetchImpl: vi.fn(async () => makeModelsResponse()) as unknown as typeof fetch,
      preserveDefaultModel: true,
      adapter: {
        read: () => config,
        write: vi.fn(),
        apply: applyManagedMirriCodeConfig,
      },
    });

    expect(result.defaultModel).toBe('mirri-code/kimi-for-coding');
    expect(result.defaultThinking).toBe(true);
    expect(config.thinking?.enabled).toBe(true);
  });

  it('preserves explicit default_thinking when preserving a custom default without capabilities', async () => {
    const config: ManagedMirriConfigShape = {
      providers: {
        custom: {
          type: 'openai',
          apiKey: 'sk-existing',
        },
      },
      defaultModel: 'custom-default',
      thinking: { enabled: true },
      models: {
        'custom-default': {
          provider: 'custom',
          model: 'custom-model',
          maxContextSize: 1000,
        },
      },
    };

    const result = await provisionManagedMirriCodeConfig({
      accessToken: 'oauth-access-token',
      fetchImpl: vi.fn(async () => makeModelsResponse()) as unknown as typeof fetch,
      preserveDefaultModel: true,
      adapter: {
        read: () => config,
        write: vi.fn(),
        apply: applyManagedMirriCodeConfig,
      },
    });

    expect(result.defaultModel).toBe('custom-default');
    expect(result.defaultThinking).toBe(true);
    expect(config.thinking?.enabled).toBe(true);
  });

  it('defaults default_thinking to false when a preserved custom default has no signal', async () => {
    const config: ManagedMirriConfigShape = {
      providers: {
        custom: {
          type: 'openai',
          apiKey: 'sk-existing',
        },
      },
      defaultModel: 'custom-default',
      models: {
        'custom-default': {
          provider: 'custom',
          model: 'custom-model',
          maxContextSize: 1000,
        },
      },
    };

    const result = await provisionManagedMirriCodeConfig({
      accessToken: 'oauth-access-token',
      fetchImpl: vi.fn(async () => makeModelsResponse()) as unknown as typeof fetch,
      preserveDefaultModel: true,
      adapter: {
        read: () => config,
        write: vi.fn(),
        apply: applyManagedMirriCodeConfig,
      },
    });

    expect(result.defaultModel).toBe('custom-default');
    expect(result.defaultThinking).toBe(false);
    expect(config.thinking?.enabled).toBe(false);
  });

  it('does not infer default_thinking from preserved custom default capabilities', async () => {
    const config: ManagedMirriConfigShape = {
      providers: {
        custom: {
          type: 'openai',
          apiKey: 'sk-existing',
        },
      },
      defaultModel: 'custom-default',
      models: {
        'custom-default': {
          provider: 'custom',
          model: 'custom-model',
          maxContextSize: 1000,
          capabilities: [],
        },
      },
    };

    const result = await provisionManagedMirriCodeConfig({
      accessToken: 'oauth-access-token',
      fetchImpl: vi.fn(async () => makeModelsResponse()) as unknown as typeof fetch,
      preserveDefaultModel: true,
      adapter: {
        read: () => config,
        write: vi.fn(),
        apply: applyManagedMirriCodeConfig,
      },
    });

    expect(result.defaultModel).toBe('custom-default');
    expect(result.defaultThinking).toBe(false);
    expect(config.thinking?.enabled).toBe(false);
  });

  it('keeps default_thinking off even when preserved custom default has thinking capability', async () => {
    const config: ManagedMirriConfigShape = {
      providers: {
        custom: {
          type: 'openai',
          apiKey: 'sk-existing',
        },
      },
      defaultModel: 'custom-default',
      models: {
        'custom-default': {
          provider: 'custom',
          model: 'custom-model',
          maxContextSize: 1000,
          capabilities: ['thinking'],
        },
      },
    };

    const result = await provisionManagedMirriCodeConfig({
      accessToken: 'oauth-access-token',
      fetchImpl: vi.fn(async () => makeModelsResponse()) as unknown as typeof fetch,
      preserveDefaultModel: true,
      adapter: {
        read: () => config,
        write: vi.fn(),
        apply: applyManagedMirriCodeConfig,
      },
    });

    expect(result.defaultModel).toBe('custom-default');
    expect(result.defaultThinking).toBe(false);
    expect(config.thinking?.enabled).toBe(false);
  });

  it('falls back to the first fetched model when the preserved default was removed', async () => {
    const config: ManagedMirriConfigShape = {
      providers: {
        [MIRRICODE_PROVIDER_NAME]: {
          type: 'openai',
          apiKey: '',
        },
      },
      defaultModel: 'mirri-code/stale',
      thinking: { enabled: false },
      models: {
        'mirri-code/stale': {
          provider: MIRRICODE_PROVIDER_NAME,
          model: 'stale',
          maxContextSize: 1000,
        },
      },
    };

    const result = await provisionManagedMirriCodeConfig({
      accessToken: 'oauth-access-token',
      fetchImpl: vi.fn(async () => makeModelsResponse()) as unknown as typeof fetch,
      preserveDefaultModel: true,
      adapter: {
        read: () => config,
        write: vi.fn(),
        apply: applyManagedMirriCodeConfig,
      },
    });

    expect(result.defaultModel).toBe('mirri-code/kimi-for-coding');
    expect(result.defaultThinking).toBe(false);
    expect(config.defaultModel).toBe('mirri-code/kimi-for-coding');
    expect(config.thinking?.enabled).toBe(false);
  });

  it('removes managed provider, models, services, and default model on logout', () => {
    const config: ManagedMirriConfigShape = {
      providers: {
        [MIRRICODE_PROVIDER_NAME]: {
          type: 'openai',
          apiKey: '',
        },
        custom: {
          type: 'openai',
          apiKey: 'sk-existing',
        },
      },
      defaultModel: 'mirri-code/kimi-for-coding',
      thinking: { enabled: true },
      models: {
        'mirri-code/kimi-for-coding': {
          provider: MIRRICODE_PROVIDER_NAME,
          model: 'kimi-for-coding',
          maxContextSize: 262144,
        },
        'custom-default': {
          provider: 'custom',
          model: 'custom-model',
          maxContextSize: 1000,
        },
      },
      services: {
        moonshotSearch: { baseUrl: 'https://api.kimi.com/coding/v1/search' },
        moonshotFetch: { baseUrl: 'https://api.kimi.com/coding/v1/fetch' },
        customService: { baseUrl: 'https://service.example.test' },
      },
      raw: {
        default_model: 'mirri-code/kimi-for-coding',
        providers: {
          [MIRRICODE_PROVIDER_NAME]: { type: 'openai' },
          custom: { type: 'openai' },
        },
        models: {
          'mirri-code/kimi-for-coding': {
            provider: MIRRICODE_PROVIDER_NAME,
            model: 'kimi-for-coding',
          },
          'custom-default': {
            provider: 'custom',
            model: 'custom-model',
          },
        },
        services: {
          moonshot_search: { base_url: 'https://api.kimi.com/coding/v1/search' },
          moonshot_fetch: { base_url: 'https://api.kimi.com/coding/v1/fetch' },
        },
      },
    };

    applyMirriManagedCodeLogoutConfig(config);

    expect(config.defaultModel).toBeUndefined();
    expect(config.providers[MIRRICODE_PROVIDER_NAME]).toBeUndefined();
    expect(config.providers['custom']).toBeDefined();
    expect(config.models?.['mirri-code/kimi-for-coding']).toBeUndefined();
    expect(config.models?.['custom-default']).toBeDefined();
    expect(config.services?.moonshotSearch).toBeUndefined();
    expect(config.services?.moonshotFetch).toBeUndefined();
    expect(config.services?.['customService']).toEqual({
      baseUrl: 'https://service.example.test',
    });
  });

  it('rejects managed models that do not include a positive context_length', async () => {
    const fetchImpl = vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            data: [{ id: 'kimi-for-coding', supports_reasoning: true }],
          }),
          { status: 200, headers: { 'Content-Type': 'application/json' } },
        ),
    ) as unknown as typeof fetch;

    await expect(
      fetchManagedMirriCodeModels({
        accessToken: 'oauth-access-token',
        fetchImpl,
      }),
    ).rejects.toThrow(/positive context_length/);
  });

  it('surfaces API error messages from model listing failures', async () => {
    const fetchImpl = vi.fn(
      async () =>
        new Response(JSON.stringify({ error: { message: 'quota exceeded' } }), {
          status: 429,
          headers: { 'Content-Type': 'application/json' },
        }),
    ) as unknown as typeof fetch;

    await expect(
      fetchManagedMirriCodeModels({
        accessToken: 'oauth-access-token',
        fetchImpl,
      }),
    ).rejects.toThrow('quota exceeded');
  });

  it('classifies model listing 401 responses as OAuth unauthorized', async () => {
    const fetchImpl = vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            error: { message: 'The API Key appears to be invalid or may have expired.' },
          }),
          {
            status: 401,
            headers: { 'Content-Type': 'application/json' },
          },
        ),
    ) as unknown as typeof fetch;

    await expect(
      fetchManagedMirriCodeModels({
        accessToken: 'oauth-access-token',
        fetchImpl,
      }),
    ).rejects.toBeInstanceOf(OAuthUnauthorizedError);
  });

  it('classifies membership-check 402 responses as OAuth unauthorized', async () => {
    const fetchImpl = vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            error: {
              message:
                "We're unable to verify your membership benefits at this time. Please ensure your membership is active.",
            },
          }),
          {
            status: 402,
            headers: { 'Content-Type': 'application/json' },
          },
        ),
    ) as unknown as typeof fetch;

    const promise = fetchManagedMirriCodeModels({
      accessToken: 'oauth-access-token',
      baseUrl: 'https://api.dev.example.test/coding/v1',
      fetchImpl,
    });

    await expect(promise).rejects.toThrow(
      "Mirri Code models endpoint https://api.dev.example.test/coding/v1 rejected OAuth credentials: We're unable to verify your membership benefits at this time. Please ensure your membership is active.",
    );
    await expect(
      fetchManagedMirriCodeModels({
        accessToken: 'oauth-access-token',
        baseUrl: 'https://api.dev.example.test/coding/v1',
        fetchImpl,
      }),
    ).rejects.toMatchObject({
      status: 402,
      baseUrl: 'https://api.dev.example.test/coding/v1',
    });
    await expect(
      fetchManagedMirriCodeModels({
        accessToken: 'oauth-access-token',
        fetchImpl,
      }),
    ).rejects.toBeInstanceOf(OAuthUnauthorizedError);
    await expect(
      fetchManagedMirriCodeModels({
        accessToken: 'oauth-access-token',
        fetchImpl,
      }),
    ).rejects.toBeInstanceOf(ManagedMirriCodeModelsAuthError);
  });

  it('clears managed provider, models, default model, and services on logout', () => {
    const config: ManagedMirriConfigShape = {
      providers: {
        [MIRRICODE_PROVIDER_NAME]: {
          type: 'openai',
          apiKey: '',
          oauth: { storage: 'file', key: 'oauth/mirri-code' },
        },
        custom: {
          type: 'openai',
          apiKey: 'sk-existing',
        },
      },
      defaultModel: 'mirri-code/kimi-for-coding',
      models: {
        'mirri-code/kimi-for-coding': {
          provider: MIRRICODE_PROVIDER_NAME,
          model: 'kimi-for-coding',
          maxContextSize: 262144,
        },
        'custom-default': {
          provider: 'custom',
          model: 'custom-model',
          maxContextSize: 128000,
        },
      },
      services: {
        moonshotSearch: {
          baseUrl: 'https://api.kimi.com/coding/v1/search',
          apiKey: '',
          oauth: { storage: 'file', key: 'oauth/mirri-code' },
        },
        moonshotFetch: {
          baseUrl: 'https://api.kimi.com/coding/v1/fetch',
          apiKey: '',
          oauth: { storage: 'file', key: 'oauth/mirri-code' },
        },
        otherService: { baseUrl: 'https://service.example.test' },
      },
    };

    const result = clearManagedMirriCodeConfig(config);

    expect(result).toMatchObject({
      providerName: MIRRICODE_PROVIDER_NAME,
      removedProvider: true,
      removedModels: ['mirri-code/kimi-for-coding'],
      defaultModelCleared: true,
      removedServices: ['moonshotSearch', 'moonshotFetch'],
    });
    expect(config.providers[MIRRICODE_PROVIDER_NAME]).toBeUndefined();
    expect(config.providers['custom']).toMatchObject({ apiKey: 'sk-existing' });
    expect(config.defaultModel).toBeUndefined();
    expect(config.models?.['mirri-code/kimi-for-coding']).toBeUndefined();
    expect(config.models?.['custom-default']).toMatchObject({ provider: 'custom' });
    expect(config.services?.moonshotSearch).toBeUndefined();
    expect(config.services?.moonshotFetch).toBeUndefined();
    expect(config.services?.['otherService']).toMatchObject({
      baseUrl: 'https://service.example.test',
    });
  });
});

describe('supports_thinking_type', () => {
  function makeThinkingTypeModelsResponse(): Response {
    return new Response(
      JSON.stringify({
        data: [
          {
            id: 'kimi-for-coding',
            context_length: 262144,
            supports_reasoning: true,
            supports_image_in: true,
            supports_video_in: true,
            supports_thinking_type: 'only',
            display_name: 'Kimi For Coding',
          },
          {
            // 'no' is the authoritative declaration and overrides the legacy
            // supports_reasoning boolean.
            id: 'kimi-plain',
            context_length: 128000,
            supports_reasoning: true,
            supports_thinking_type: 'no',
          },
          {
            id: 'kimi-toggle',
            context_length: 128000,
            supports_reasoning: true,
            supports_thinking_type: 'both',
          },
        ],
      }),
      { status: 200, headers: { 'Content-Type': 'application/json' } },
    );
  }

  it('parses supports_thinking_type from the models endpoint', async () => {
    const models = await fetchManagedMirriCodeModels({
      accessToken: 'oauth-access-token',
      fetchImpl: vi.fn(async () => makeThinkingTypeModelsResponse()) as unknown as typeof fetch,
    });

    expect(models[0]?.supportsThinkingType).toBe('only');
    expect(models[1]?.supportsThinkingType).toBe('no');
    expect(models[2]?.supportsThinkingType).toBe('both');
  });

  it('leaves supportsThinkingType undefined when the field is absent or invalid', async () => {
    const absent = await fetchManagedMirriCodeModels({
      accessToken: 'oauth-access-token',
      fetchImpl: vi.fn(async () => makeModelsResponse()) as unknown as typeof fetch,
    });
    expect(absent[0]?.supportsThinkingType).toBeUndefined();

    const invalid = await fetchManagedMirriCodeModels({
      accessToken: 'oauth-access-token',
      fetchImpl: vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              data: [
                {
                  id: 'kimi-for-coding',
                  context_length: 262144,
                  supports_reasoning: true,
                  supports_thinking_type: 'maybe',
                },
              ],
            }),
            { status: 200, headers: { 'Content-Type': 'application/json' } },
          ),
      ) as unknown as typeof fetch,
    });
    expect(invalid[0]?.supportsThinkingType).toBeUndefined();
  });

  it('maps the three states onto capabilities, overriding supports_reasoning', async () => {
    const config: ManagedMirriConfigShape = { providers: {} };

    await provisionManagedMirriCodeConfig({
      accessToken: 'oauth-access-token',
      fetchImpl: vi.fn(async () => makeThinkingTypeModelsResponse()) as unknown as typeof fetch,
      adapter: {
        read: () => config,
        write: vi.fn(),
        apply: applyManagedMirriCodeConfig,
      },
    });

    // 'only' → thinking locked on.
    expect(config.models?.['mirri-code/kimi-for-coding']?.capabilities).toEqual([
      'thinking',
      'always_thinking',
      'image_in',
      'video_in',
      'tool_use',
    ]);
    // 'no' → no thinking capability despite supports_reasoning=true.
    expect(config.models?.['mirri-code/kimi-plain']?.capabilities).toEqual(['tool_use']);
    // 'both' → plain toggleable thinking.
    expect(config.models?.['mirri-code/kimi-toggle']?.capabilities).toEqual([
      'thinking',
      'tool_use',
    ]);
  });

  it('forces default thinking on when the selected default model is thinking-only', async () => {
    const config: ManagedMirriConfigShape = { providers: {}, thinking: { enabled: false } };

    const result = await provisionManagedMirriCodeConfig({
      accessToken: 'oauth-access-token',
      fetchImpl: vi.fn(async () => makeThinkingTypeModelsResponse()) as unknown as typeof fetch,
      adapter: {
        read: () => config,
        write: vi.fn(),
        apply: applyManagedMirriCodeConfig,
      },
    });

    expect(result.defaultModel).toBe('mirri-code/kimi-for-coding');
    expect(result.defaultThinking).toBe(true);
    expect(config.thinking?.enabled).toBe(true);
  });

  it('forces default thinking on when preserving a thinking-only managed default', async () => {
    const config: ManagedMirriConfigShape = {
      providers: {
        [MIRRICODE_PROVIDER_NAME]: {
          type: 'openai',
          apiKey: '',
        },
      },
      defaultModel: 'mirri-code/kimi-for-coding',
      thinking: { enabled: false },
      models: {
        'mirri-code/kimi-for-coding': {
          provider: MIRRICODE_PROVIDER_NAME,
          model: 'kimi-for-coding',
          maxContextSize: 262144,
          capabilities: ['thinking'],
        },
      },
    };

    const result = await provisionManagedMirriCodeConfig({
      accessToken: 'oauth-access-token',
      fetchImpl: vi.fn(async () => makeThinkingTypeModelsResponse()) as unknown as typeof fetch,
      preserveDefaultModel: true,
      adapter: {
        read: () => config,
        write: vi.fn(),
        apply: applyManagedMirriCodeConfig,
      },
    });

    expect(result.defaultModel).toBe('mirri-code/kimi-for-coding');
    expect(result.defaultThinking).toBe(true);
    expect(config.thinking?.enabled).toBe(true);
  });

  it('forces default thinking off when preserving a no-thinking managed default', async () => {
    const config: ManagedMirriConfigShape = {
      providers: {
        [MIRRICODE_PROVIDER_NAME]: {
          type: 'openai',
          apiKey: '',
        },
      },
      defaultModel: 'mirri-code/kimi-plain',
      thinking: { enabled: true },
      models: {
        'mirri-code/kimi-plain': {
          provider: MIRRICODE_PROVIDER_NAME,
          model: 'kimi-plain',
          maxContextSize: 128000,
          capabilities: ['thinking'],
        },
      },
    };

    const result = await provisionManagedMirriCodeConfig({
      accessToken: 'oauth-access-token',
      fetchImpl: vi.fn(async () => makeThinkingTypeModelsResponse()) as unknown as typeof fetch,
      preserveDefaultModel: true,
      adapter: {
        read: () => config,
        write: vi.fn(),
        apply: applyManagedMirriCodeConfig,
      },
    });

    expect(result.defaultModel).toBe('mirri-code/kimi-plain');
    expect(result.defaultThinking).toBe(false);
    expect(config.thinking?.enabled).toBe(false);
  });

  it('keeps a preserved non-managed default thinking selection untouched', async () => {
    const config: ManagedMirriConfigShape = {
      providers: {
        custom: {
          type: 'openai',
          apiKey: 'sk-existing',
        },
      },
      defaultModel: 'custom-default',
      thinking: { enabled: false },
      models: {
        'custom-default': {
          provider: 'custom',
          model: 'custom-model',
          maxContextSize: 1000,
        },
      },
    };

    const result = await provisionManagedMirriCodeConfig({
      accessToken: 'oauth-access-token',
      fetchImpl: vi.fn(async () => makeThinkingTypeModelsResponse()) as unknown as typeof fetch,
      preserveDefaultModel: true,
      adapter: {
        read: () => config,
        write: vi.fn(),
        apply: applyManagedMirriCodeConfig,
      },
    });

    expect(result.defaultModel).toBe('custom-default');
    expect(result.defaultThinking).toBe(false);
    expect(config.thinking?.enabled).toBe(false);
  });
});

describe('support_efforts / default_effort', () => {
  function makeEffortModelsResponse(): Response {
    return new Response(
      JSON.stringify({
        data: [
          {
            id: 'kimi-for-coding',
            context_length: 262144,
            supports_reasoning: true,
            supports_thinking_type: 'both',
            think_efforts: {
              support: true,
              valid_efforts: ['low', 'high', 'max'],
              default_effort: 'high',
            },
            display_name: 'Kimi For Coding',
          },
          {
            // Empty / non-string entries are filtered; absent fields stay undefined.
            id: 'kimi-plain',
            context_length: 128000,
            supports_reasoning: true,
            think_efforts: { support: true, valid_efforts: ['low', '', 42] },
          },
        ],
      }),
      { status: 200, headers: { 'Content-Type': 'application/json' } },
    );
  }

  it('parses think_efforts from the models endpoint', async () => {
    const models = await fetchManagedMirriCodeModels({
      accessToken: 'oauth-access-token',
      fetchImpl: vi.fn(async () => makeEffortModelsResponse()) as unknown as typeof fetch,
    });

    expect(models[0]?.supportEfforts).toEqual(['low', 'high', 'max']);
    expect(models[0]?.defaultEffort).toBe('high');
    // The empty string and number are filtered out of valid_efforts.
    expect(models[1]?.supportEfforts).toEqual(['low']);
    expect(models[1]?.defaultEffort).toBeUndefined();
  });

  it('ignores think_efforts entirely when support is not true', async () => {
    const models = await fetchManagedMirriCodeModels({
      accessToken: 'oauth-access-token',
      fetchImpl: async () =>
        new Response(
          JSON.stringify({
            data: [
              {
                id: 'kimi-no-effort',
                context_length: 128000,
                supports_reasoning: true,
                think_efforts: {
                  support: false,
                  valid_efforts: ['low', 'high'],
                  default_effort: 'high',
                },
              },
            ],
          }),
          { status: 200, headers: { 'Content-Type': 'application/json' } },
        ),
    });

    // support !== true gates the whole object — valid_efforts / default_effort
    // are ignored.
    expect(models[0]?.supportEfforts).toBeUndefined();
    expect(models[0]?.defaultEffort).toBeUndefined();
  });

  it('ignores legacy flat fields even when think_efforts is absent', async () => {
    // The legacy support_efforts / default_effort fields are no longer read;
    // only the nested think_efforts object is honored.
    const models = await fetchManagedMirriCodeModels({
      accessToken: 'oauth-access-token',
      fetchImpl: async () =>
        new Response(
          JSON.stringify({
            data: [
              {
                id: 'kimi-k2',
                context_length: 128000,
                supports_reasoning: true,
                support_efforts: ['low', 'high'],
                default_effort: 'high',
              },
            ],
          }),
          { status: 200, headers: { 'Content-Type': 'application/json' } },
        ),
    });

    expect(models[0]?.supportEfforts).toBeUndefined();
    expect(models[0]?.defaultEffort).toBeUndefined();
  });

  it('writes supportEfforts and defaultEffort onto the provisioned model entry', async () => {
    const config: ManagedMirriConfigShape = { providers: {} };

    await provisionManagedMirriCodeConfig({
      accessToken: 'oauth-access-token',
      fetchImpl: vi.fn(async () => makeEffortModelsResponse()) as unknown as typeof fetch,
      adapter: {
        read: () => config,
        write: vi.fn(),
        apply: applyManagedMirriCodeConfig,
      },
    });

    const alias = config.models?.['mirri-code/kimi-for-coding'];
    expect(alias?.['supportEfforts']).toEqual(['low', 'high', 'max']);
    expect(alias?.['defaultEffort']).toBe('high');
  });
});

describe('selective merge', () => {
  const baseOptions = {
    baseUrl: 'https://api.example.test/coding/v1',
    oauthKey: 'test-key',
  };

  it('preserves non-managed user fields and does not modify existing aliases', () => {
    const config: ManagedMirriConfigShape = {
      providers: {},
      models: {
        'mirri-code/kimi-k2': {
          provider: 'mirri-code',
          model: 'kimi-k2',
          maxContextSize: 262144,
          capabilities: ['thinking'],
          maxOutputSize: 4096,
          supportEfforts: ['low', 'high', 'max'],
        } as Record<string, unknown>,
      },
    };

    applyManagedMirriCodeConfig(config, {
      ...baseOptions,
      models: [
        {
          id: 'kimi-k2',
          contextLength: 262144,
          supportsReasoning: true,
          supportsImageIn: false,
          supportsVideoIn: false,
          supportsThinkingType: 'both',
        },
      ],
    });

    const alias = config.models?.['mirri-code/kimi-k2'];
    // Existing alias is untouched — append-only refresh.
    expect(alias?.['maxOutputSize']).toBe(4096);
    expect(alias?.['supportEfforts']).toEqual(['low', 'high', 'max']);
    expect(alias?.['maxContextSize']).toBe(262144);
  });

  it('does not modify an existing alias even when upstream re-declares managed fields', () => {
    const config: ManagedMirriConfigShape = {
      providers: {},
      models: {
        'mirri-code/kimi-k2': {
          provider: 'mirri-code',
          model: 'kimi-k2',
          maxContextSize: 262144,
        } as Record<string, unknown>,
      },
    };

    applyManagedMirriCodeConfig(config, {
      ...baseOptions,
      models: [
        {
          id: 'kimi-k2',
          contextLength: 262144,
          supportsReasoning: true,
          supportsImageIn: false,
          supportsVideoIn: false,
          supportEfforts: ['low', 'high', 'max'],
          defaultEffort: 'high',
        },
      ],
    });

    const alias = config.models?.['mirri-code/kimi-k2'];
    // Existing alias fields are preserved; upstream values do not overwrite.
    expect(alias?.['supportEfforts']).toBeUndefined();
    expect(alias?.['defaultEffort']).toBeUndefined();
    expect(alias?.['maxContextSize']).toBe(262144);
  });

  it('keeps managed models that upstream no longer lists', () => {
    const config: ManagedMirriConfigShape = {
      providers: {},
      models: {
        'mirri-code/kimi-k2': {
          provider: MIRRICODE_PROVIDER_NAME,
          model: 'kimi-k2',
          maxContextSize: 262144,
        },
        'mirri-code/removed': {
          provider: MIRRICODE_PROVIDER_NAME,
          model: 'removed',
          maxContextSize: 128000,
        },
      },
    };

    applyManagedMirriCodeConfig(config, {
      ...baseOptions,
      models: [
        {
          id: 'kimi-k2',
          contextLength: 262144,
          supportsReasoning: true,
          supportsImageIn: false,
          supportsVideoIn: false,
        },
      ],
    });

    expect(config.models?.['mirri-code/kimi-k2']).toBeDefined();
    // Removed-upstream model is preserved (append-only refresh).
    expect(config.models?.['mirri-code/removed']).toMatchObject({
      provider: MIRRICODE_PROVIDER_NAME,
      model: 'removed',
    });
  });
});

function makeModelInfo(
  id: string,
  overrides: Partial<ManagedMirriCodeModelInfo> = {},
): ManagedMirriCodeModelInfo {
  return {
    id,
    contextLength: 200000,
    supportsReasoning: false,
    supportsImageIn: false,
    supportsVideoIn: false,
    ...overrides,
  };
}

const KIMI_BASE_URL = 'https://api.kimi.com/coding/v1';

describe('managed protocol routing', () => {
  it('reads protocol from the /models response', async () => {
    const fetchImpl = vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            data: [{ id: 'kimi-for-coding', context_length: 262144, protocol: 'anthropic' }],
          }),
          { status: 200, headers: { 'Content-Type': 'application/json' } },
        ),
    ) as unknown as typeof fetch;

    const models = await fetchManagedMirriCodeModels({ accessToken: 't', fetchImpl });
    expect(models).toHaveLength(1);
    expect(models[0]?.protocol).toBe('anthropic');
  });

  it('keeps the provider on the kimi REST base and records the model protocol when anthropic', () => {
    const config: ManagedMirriConfigShape = { providers: {} };
    applyManagedMirriCodeConfig(config, {
      baseUrl: KIMI_BASE_URL,
      models: [makeModelInfo('kimi-for-coding', { protocol: 'anthropic' })],
    });

    // The provider stays on the kimi wire + REST base; the anthropic transport
    // is resolved per-model at runtime, not baked into the provider config, so
    // the REST base keeps flowing to OAuth key derivation and plugin env.
    expect(config.providers[MIRRICODE_PROVIDER_NAME]).toMatchObject({
      type: 'openai',
      baseUrl: KIMI_BASE_URL,
      apiKey: '',
    });
    expect(config.models?.['mirri-code/kimi-for-coding']).toMatchObject({
      provider: MIRRICODE_PROVIDER_NAME,
      protocol: 'anthropic',
      betaApi: true,
    });
  });

  it('keeps the kimi protocol and baseUrl when the model has no anthropic protocol', () => {
    const config: ManagedMirriConfigShape = { providers: {} };
    applyManagedMirriCodeConfig(config, {
      baseUrl: KIMI_BASE_URL,
      models: [makeModelInfo('kimi-for-coding')],
    });

    expect(config.providers[MIRRICODE_PROVIDER_NAME]).toMatchObject({
      type: 'openai',
      baseUrl: KIMI_BASE_URL,
      apiKey: '',
    });
    expect(config.models?.['mirri-code/kimi-for-coding']?.provider).toBe(MIRRICODE_PROVIDER_NAME);
    expect(config.models?.['mirri-code/kimi-for-coding']?.protocol).toBeUndefined();
  });

  it('does not modify an existing alias protocol on refresh when the server stops declaring anthropic', () => {
    const config: ManagedMirriConfigShape = {
      providers: {},
      models: {
        'mirri-code/kimi-for-coding': {
          provider: MIRRICODE_PROVIDER_NAME,
          model: 'kimi-for-coding',
          maxContextSize: 200000,
          protocol: 'anthropic',
          betaApi: true,
        },
      },
    };
    // First refresh sets the protocol on a fresh alias — verify the baseline.
    expect(config.models?.['mirri-code/kimi-for-coding']?.protocol).toBe('anthropic');

    applyManagedMirriCodeConfig(config, {
      baseUrl: KIMI_BASE_URL,
      models: [makeModelInfo('kimi-for-coding')],
    });
    // The provider never leaves the kimi wire / REST base across refreshes.
    expect(config.providers[MIRRICODE_PROVIDER_NAME]).toMatchObject({
      type: 'openai',
      baseUrl: KIMI_BASE_URL,
    });
    // Existing alias is untouched (append-only refresh).
    expect(config.models?.['mirri-code/kimi-for-coding']?.protocol).toBe('anthropic');
  });
});
