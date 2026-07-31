import { describe, expect, it } from 'vitest';

import type { MirriConfig, ModelAlias } from '../../src/config';
import { ErrorCodes, MirriError } from '../../src/errors';
import { ProviderManager } from '../../src/session/provider-manager';
import { resolveThinkingEffort } from '../../src/agent/config/thinking';

// Thin wrapper that adapts the legacy `resolveRuntimeProvider(input)` shape to
// the current ProviderManager API. Kept local so the existing test bodies do
// not need to change.
function resolveRuntimeProvider(input: {
  readonly config: MirriConfig;
  readonly model?: string;
  readonly mirriRequestHeaders?: Record<string, string>;
  readonly promptCacheKey?: string;
}): ReturnType<ProviderManager['resolveProviderConfig']> {
  const manager = new ProviderManager({
    config: input.config,
    mirriRequestHeaders: input.mirriRequestHeaders,
    promptCacheKey: input.promptCacheKey,
  });
  const model = input.model ?? input.config.defaultModel;
  if (model === undefined) {
    throw new MirriError(
      ErrorCodes.CONFIG_INVALID,
      'No model is selected. Set default_model in config.toml or pass a configured model alias.',
    );
  }
  return manager.resolveProviderConfig(model);
}

const BASE_CONFIG: MirriConfig = {
  defaultModel: 'mirri-code/kimi-for-coding',
  providers: {
    'managed:mirri-code': {
      type: 'openai',
      apiKey: 'test-key',
      baseUrl: 'https://api.example/v1',
    },
  },
  models: {
    'mirri-code/kimi-for-coding': {
      provider: 'managed:mirri-code',
      model: 'kimi-for-coding',
      maxContextSize: 1_000_000,
      capabilities: ['thinking', 'image_in', 'video_in', 'tool_use'],
    },
  },
};

const TEST_MIRRI_HEADERS = {
  'User-Agent': 'mirri-code-cli/0.0.0-test',
  'X-Msh-Platform': 'kimi_code_cli',
  'X-Msh-Version': '0.0.0-test',
};

describe('resolveRuntimeProvider model metadata', () => {
  it('uses config model metadata as the source of truth', () => {
    const resolved = resolveRuntimeProvider({
      config: BASE_CONFIG,
    });

    expect(resolved.modelCapabilities).toMatchObject({
      image_in: true,
      video_in: true,
      thinking: true,
      tool_use: true,
      max_context_tokens: 1_000_000,
    });
    expect(resolved.provider.model).toBe('kimi-for-coding');
  });

  it('resolves requested aliases to the configured provider and provider model', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        ...BASE_CONFIG,
        providers: {
          ...BASE_CONFIG.providers,
          openai: {
            type: 'openai',
            apiKey: 'sk-openai',
            baseUrl: 'https://openai.example/v1',
          },
        },
        models: {
          ...BASE_CONFIG.models!,
          'gpt-alias': {
            provider: 'openai',
            model: 'gpt-runtime',
            maxContextSize: 200000,
            capabilities: ['tool_use'],
          },
        },
      },
      model: 'gpt-alias',
    });

    expect(resolved.providerName).toBe('openai');
    expect(resolved.provider).toMatchObject({
      type: 'openai',
      model: 'gpt-runtime',
      apiKey: 'sk-openai',
      baseUrl: 'https://openai.example/v1',
    });
    expect(resolved.modelCapabilities).toMatchObject({
      tool_use: true,
      max_context_tokens: 200000,
    });
  });

  it('uses config Mirri capabilities without requiring an api key during OAuth setup', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        ...BASE_CONFIG,
        providers: {
          'managed:mirri-code': {
            type: 'openai',
            apiKey: '',
            baseUrl: 'https://api.example/v1',
            oauth: { storage: 'file', key: 'oauth/mirri-code' },
          },
        },
      },
    });

    expect(resolved.modelCapabilities).toMatchObject({
      image_in: true,
      video_in: true,
      thinking: true,
      tool_use: true,
      max_context_tokens: 1_000_000,
    });
  });

  it('does not infer Mirri capabilities from the provider model name', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        ...BASE_CONFIG,
        models: {
          'mirri-code/kimi-for-coding': {
            provider: 'managed:mirri-code',
            model: 'kimi-for-coding',
            maxContextSize: 1_000_000,
          },
        },
      },
    });

    expect(resolved.modelCapabilities).toMatchObject({
      image_in: false,
      video_in: false,
      thinking: false,
      tool_use: false,
      max_context_tokens: 1_000_000,
    });
  });

  it('rejects provider model names that are not configured aliases', () => {
    expect(() =>
      resolveRuntimeProvider({
        config: BASE_CONFIG,
        model: 'kimi-for-coding',
      }),
    ).toThrow(/not configured in config.toml/);
  });

  it('throws when no model is selected', () => {
    expect(() =>
      resolveRuntimeProvider({
        config: {
          providers: {},
        },
      }),
    ).toThrow(/No model is selected/);
  });

  it('throws when the selected model is not configured as an alias', () => {
    expect(() =>
      resolveRuntimeProvider({
        config: BASE_CONFIG,
        model: 'mirri-code',
      }),
    ).toThrow(MirriError);
  });

  it('allows vertexai providers without an apiKey', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        defaultModel: 'gemini',
        providers: {
          vertex: {
            type: 'vertexai',
          },
        },
        models: {
          gemini: {
            provider: 'vertex',
            model: 'gemini-1.5-pro',
            maxContextSize: 1_000_000,
          },
        },
      },
    });

    expect(resolved.provider).toMatchObject({ type: 'vertexai' });
  });

  it('throws when the selected model alias has no maxContextSize', () => {
    const config = {
      ...BASE_CONFIG,
      models: {
        broken: {
          provider: 'managed:mirri-code',
          model: 'kimi-for-coding',
          capabilities: ['thinking'],
        },
      },
    } as unknown as MirriConfig;

    expect(() =>
      resolveRuntimeProvider({
        config,
        model: 'broken',
      }),
    ).toThrow(/max_context_size/);
  });
});

describe('resolveRuntimeProvider maxOutputSize forwarding', () => {
  it('returns alias.maxOutputSize for request completion budgeting', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        ...BASE_CONFIG,
        providers: {
          ...BASE_CONFIG.providers,
          openai: {
            type: 'openai',
            apiKey: 'sk-openai',
            baseUrl: 'https://openai.example/v1',
          },
        },
        models: {
          ...BASE_CONFIG.models!,
          'deepseek-alias': {
            provider: 'openai',
            model: 'deepseek-v4-flash',
            maxContextSize: 1_000_000,
            maxOutputSize: 384000,
          },
        },
      },
      model: 'deepseek-alias',
    });

    expect(resolved.maxOutputSize).toBe(384000);
  });

  it('forwards alias.maxOutputSize to the anthropic provider config as defaultMaxTokens', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        ...BASE_CONFIG,
        providers: {
          ...BASE_CONFIG.providers,
          anthropic: { type: 'anthropic', apiKey: 'sk-anthropic' },
        },
        models: {
          ...BASE_CONFIG.models!,
          'opus-alias': {
            provider: 'anthropic',
            model: 'claude-opus-4-7',
            maxContextSize: 200000,
            maxOutputSize: 24000,
          },
        },
      },
      model: 'opus-alias',
    });

    expect(resolved.provider).toMatchObject({
      type: 'anthropic',
      model: 'claude-opus-4-7',
      defaultMaxTokens: 24000,
    });
  });

  it('omits defaultMaxTokens when alias.maxOutputSize is unset', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        ...BASE_CONFIG,
        providers: {
          ...BASE_CONFIG.providers,
          anthropic: { type: 'anthropic', apiKey: 'sk-anthropic' },
        },
        models: {
          ...BASE_CONFIG.models!,
          'opus-alias': {
            provider: 'anthropic',
            model: 'claude-opus-4-7',
            maxContextSize: 200000,
          },
        },
      },
      model: 'opus-alias',
    });

    expect(resolved.provider).toMatchObject({
      type: 'anthropic',
      model: 'claude-opus-4-7',
    });
    expect('defaultMaxTokens' in resolved.provider).toBe(false);
  });

  it('forwards alias.adaptiveThinking to the anthropic provider config', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        ...BASE_CONFIG,
        providers: {
          ...BASE_CONFIG.providers,
          anthropic: { type: 'anthropic', apiKey: 'sk-anthropic' },
        },
        models: {
          ...BASE_CONFIG.models!,
          'okapi-alias': {
            provider: 'anthropic',
            model: 'coding-model-okapi-0527-vibe',
            maxContextSize: 200000,
            adaptiveThinking: true,
          },
        },
      },
      model: 'okapi-alias',
    });

    expect(resolved.provider).toMatchObject({
      type: 'anthropic',
      model: 'coding-model-okapi-0527-vibe',
      adaptiveThinking: true,
    });
  });

  it('forwards alias.betaApi to the anthropic provider config', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        ...BASE_CONFIG,
        providers: {
          ...BASE_CONFIG.providers,
          anthropic: { type: 'anthropic', apiKey: 'sk-anthropic' },
        },
        models: {
          ...BASE_CONFIG.models!,
          'kimi-alias': {
            provider: 'anthropic',
            model: 'kimi-for-coding',
            maxContextSize: 200000,
            protocol: 'anthropic',
            betaApi: true,
          },
        },
      },
      model: 'kimi-alias',
    });

    expect(resolved.provider).toMatchObject({
      type: 'anthropic',
      model: 'kimi-for-coding',
      betaApi: true,
    });
  });

  it('omits adaptiveThinking when alias.adaptiveThinking is unset', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        ...BASE_CONFIG,
        providers: {
          ...BASE_CONFIG.providers,
          anthropic: { type: 'anthropic', apiKey: 'sk-anthropic' },
        },
        models: {
          ...BASE_CONFIG.models!,
          'opus-alias': {
            provider: 'anthropic',
            model: 'claude-opus-4-7',
            maxContextSize: 200000,
          },
        },
      },
      model: 'opus-alias',
    });

    expect('adaptiveThinking' in resolved.provider).toBe(false);
  });
});

describe('resolveRuntimeProvider Mirri request headers', () => {
  it('does not set defaultHeaders when no mirriRequestHeaders or customHeaders exist', () => {
    const resolved = resolveRuntimeProvider({ config: BASE_CONFIG });

    expect(resolved.provider).toMatchObject({
      type: 'openai',
      model: 'kimi-for-coding',
    });
    expect('defaultHeaders' in resolved.provider).toBe(false);
  });

  it('uses only customHeaders when mirriRequestHeaders are missing', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        ...BASE_CONFIG,
        providers: {
          'managed:mirri-code': {
            type: 'openai',
            apiKey: 'test-key',
            baseUrl: 'https://api.example/v1',
            customHeaders: {
              'User-Agent': 'Custom/1',
            },
          },
        },
      },
    });

    expect(resolved.provider).toMatchObject({
      type: 'openai',
      defaultHeaders: {
        'User-Agent': 'Custom/1',
      },
    });
  });

  it('passes mirriRequestHeaders through to Mirri provider defaultHeaders', () => {
    const resolved = resolveRuntimeProvider({
      config: BASE_CONFIG,
      mirriRequestHeaders: TEST_MIRRI_HEADERS,
    });

    expect(resolved.provider).toMatchObject({
      type: 'openai',
      defaultHeaders: {
        'User-Agent': TEST_MIRRI_HEADERS['User-Agent'],
      },
    });
  });

  it('passes the prompt cache key to Mirri generation kwargs', () => {
    const resolved = resolveRuntimeProvider({
      config: BASE_CONFIG,
      promptCacheKey: 'session-test',
    });

    expect(resolved.provider).toMatchObject({
      type: 'openai',
      generationKwargs: {
        prompt_cache_key: 'session-test',
      },
    });
  });

  it('lets provider customHeaders override mirriRequestHeaders', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        ...BASE_CONFIG,
        providers: {
          'managed:mirri-code': {
            type: 'openai',
            apiKey: 'test-key',
            baseUrl: 'https://api.example/v1',
            customHeaders: {
              'User-Agent': 'Custom/1',
              'X-Msh-Version': 'override-version',
            },
          },
        },
      },
      mirriRequestHeaders: TEST_MIRRI_HEADERS,
    });

    expect(resolved.provider).toMatchObject({
      type: 'openai',
      defaultHeaders: {
        'User-Agent': 'Custom/1',
        'X-Msh-Version': 'override-version',
      },
    });
  });

  it('applies only the User-Agent from mirriRequestHeaders to non-Mirri providers', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        defaultModel: 'gpt-alias',
        providers: {
          openai: {
            type: 'openai',
            apiKey: 'sk-openai',
            baseUrl: 'https://api.openai.com/v1',
          },
        },
        models: {
          'gpt-alias': {
            provider: 'openai',
            model: 'gpt-runtime',
            maxContextSize: 200000,
          },
        },
      },
      mirriRequestHeaders: TEST_MIRRI_HEADERS,
      promptCacheKey: 'session-test',
    });

    expect(resolved.provider).toMatchObject({
      type: 'openai',
      model: 'gpt-runtime',
      apiKey: 'sk-openai',
      defaultHeaders: {
        'User-Agent': TEST_MIRRI_HEADERS['User-Agent'],
      },
    });
    // Device identity headers (`X-Msh-*`) stay Mirri-only — they must not leak
    // to third-party providers.
    const headers = (resolved.provider as { defaultHeaders?: Record<string, string> })
      .defaultHeaders;
    expect(headers).toBeDefined();
    expect('X-Msh-Platform' in headers!).toBe(false);
    expect(resolved.provider).toMatchObject({
      generationKwargs: {
        prompt_cache_key: 'session-test',
      },
    });
  });
});

describe('resolveRuntimeProvider customHeaders propagation', () => {
  it('forwards customHeaders to an anthropic provider', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        defaultModel: 'claude-alias',
        providers: {
          anthropic: {
            type: 'anthropic',
            apiKey: 'sk-anthropic',
            customHeaders: { 'X-Custom': 'value' },
          },
        },
        models: {
          'claude-alias': { provider: 'anthropic', model: 'claude-runtime', maxContextSize: 200000 },
        },
      },
    });

    expect(resolved.provider).toMatchObject({
      type: 'anthropic',
      defaultHeaders: { 'X-Custom': 'value' },
    });
  });

  it('passes the prompt cache key to Anthropic metadata.user_id', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        defaultModel: 'claude-alias',
        providers: {
          anthropic: {
            type: 'anthropic',
            apiKey: 'sk-anthropic',
          },
        },
        models: {
          'claude-alias': { provider: 'anthropic', model: 'claude-runtime', maxContextSize: 200000 },
        },
      },
      promptCacheKey: 'session-test',
    });

    expect(resolved.provider).toMatchObject({
      type: 'anthropic',
      metadata: { user_id: 'session-test' },
    });
  });

  it('omits Anthropic metadata when no prompt cache key is set', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        defaultModel: 'claude-alias',
        providers: {
          anthropic: {
            type: 'anthropic',
            apiKey: 'sk-anthropic',
          },
        },
        models: {
          'claude-alias': { provider: 'anthropic', model: 'claude-runtime', maxContextSize: 200000 },
        },
      },
    });

    expect(resolved.provider).toMatchObject({ type: 'anthropic' });
    expect('metadata' in resolved.provider).toBe(false);
  });

  it('forwards customHeaders to an openai provider', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        defaultModel: 'gpt-alias',
        providers: {
          openai: {
            type: 'openai',
            apiKey: 'sk-openai',
            customHeaders: { 'X-Custom': 'value' },
          },
        },
        models: {
          'gpt-alias': { provider: 'openai', model: 'gpt-runtime', maxContextSize: 200000 },
        },
      },
    });

    expect(resolved.provider).toMatchObject({
      type: 'openai',
      defaultHeaders: { 'X-Custom': 'value' },
    });
  });

  it('forwards customHeaders to an openai_responses provider', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        defaultModel: 'resp-alias',
        providers: {
          openai_responses: {
            type: 'openai_responses',
            apiKey: 'sk-openai',
            customHeaders: { 'X-Custom': 'value' },
          },
        },
        models: {
          'resp-alias': {
            provider: 'openai_responses',
            model: 'gpt-runtime',
            maxContextSize: 200000,
          },
        },
      },
    });

    expect(resolved.provider).toMatchObject({
      type: 'openai_responses',
      defaultHeaders: { 'X-Custom': 'value' },
    });
  });

  it('keeps customHeaders isolated between resolved provider instances', () => {
    const config: MirriConfig = {
      defaultModel: 'gpt-alias',
      providers: {
        openai: {
          type: 'openai',
          apiKey: 'sk-openai',
          customHeaders: { 'X-Custom': 'original' },
        },
      },
      models: {
        'gpt-alias': { provider: 'openai', model: 'gpt-runtime', maxContextSize: 200000 },
      },
    };

    const first = resolveRuntimeProvider({ config });
    const second = resolveRuntimeProvider({ config });
    const firstHeaders = (first.provider as { defaultHeaders?: Record<string, string> })
      .defaultHeaders;
    expect(firstHeaders).toEqual({ 'X-Custom': 'original' });

    firstHeaders!['X-Custom'] = 'mutated';

    expect(
      (second.provider as { defaultHeaders?: Record<string, string> }).defaultHeaders,
    ).toEqual({ 'X-Custom': 'original' });
    expect(config.providers['openai']?.customHeaders).toEqual({ 'X-Custom': 'original' });
  });
});

describe('ProviderManager prompt cache key', () => {
  it('applies a prompt cache key to Mirri providers', () => {
    const manager = new ProviderManager({
      config: BASE_CONFIG,
      promptCacheKey: 'session-test',
    });
    const resolved = manager.resolveProviderConfig('mirri-code/kimi-for-coding');

    expect(resolved.provider).toMatchObject({
      type: 'openai',
      generationKwargs: {
        prompt_cache_key: 'session-test',
      },
    });
  });

  it('adds generation kwargs to openai providers with a prompt cache key', () => {
    const manager = new ProviderManager({
      promptCacheKey: 'session-test',
      config: {
        defaultModel: 'gpt-alias',
        providers: {
          openai: {
            type: 'openai',
            apiKey: 'sk-openai',
          },
        },
        models: {
          'gpt-alias': {
            provider: 'openai',
            model: 'gpt-runtime',
            maxContextSize: 200000,
          },
        },
      },
    });
    const resolved = manager.resolveProviderConfig('gpt-alias');

    expect(resolved.provider).toMatchObject({
      type: 'openai',
      model: 'gpt-runtime',
      generationKwargs: {
        prompt_cache_key: 'session-test',
      },
    });
  });

  it('reads the current config when constructed with a function', () => {
    let sharedConfig: MirriConfig = { providers: {} };
    const manager = new ProviderManager({
      config: () => sharedConfig,
      promptCacheKey: 'session-test',
    });

    sharedConfig = BASE_CONFIG;

    const resolved = manager.resolveProviderConfig('mirri-code/kimi-for-coding');
    expect(resolved.provider).toMatchObject({
      type: 'openai',
      generationKwargs: {
        prompt_cache_key: 'session-test',
      },
    });
  });
});

describe('ProviderManager OAuth auth', () => {
  function oauthConfig(): MirriConfig {
    return {
      ...BASE_CONFIG,
      providers: {
        'managed:mirri-code': {
          type: 'openai',
          apiKey: '',
          baseUrl: 'https://api.example/v1',
          oauth: { storage: 'file', key: 'oauth/mirri-code' },
        },
      },
    };
  }

  it('preserves non-Mirri token fetch failures instead of guessing their category', async () => {
    const tokenError = new Error('token storage permission denied');
    const manager = new ProviderManager({
      config: oauthConfig(),
      resolveOAuthTokenProvider: () => ({
        async getAccessToken() {
          throw tokenError;
        },
      }),
    });

    const resolveAuth = manager.resolveAuth('mirri-code/kimi-for-coding');
    expect(resolveAuth).toBeDefined();

    await expect(resolveAuth!(async () => 'ok')).rejects.toBe(tokenError);
  });

  it('keeps explicit login-required token failures as login-required errors', async () => {
    const manager = new ProviderManager({
      config: oauthConfig(),
      resolveOAuthTokenProvider: () => ({
        async getAccessToken() {
          throw new MirriError(ErrorCodes.AUTH_LOGIN_REQUIRED, 'not logged in');
        },
      }),
    });

    const resolveAuth = manager.resolveAuth('mirri-code/kimi-for-coding');
    expect(resolveAuth).toBeDefined();

    await expect(resolveAuth!(async () => 'ok')).rejects.toMatchObject({
      code: ErrorCodes.AUTH_LOGIN_REQUIRED,
    });
  });
});

describe('resolveThinkingEffort', () => {
  const booleanModel: ModelAlias = {
    provider: 'p',
    model: 'm',
    maxContextSize: 1,
    capabilities: ['thinking'],
  };
  const effortModel: ModelAlias = {
    provider: 'p',
    model: 'm',
    maxContextSize: 1,
    capabilities: ['thinking'],
    supportEfforts: ['low', 'medium', 'high'],
  };
  const alwaysThinkingModel: ModelAlias = {
    provider: 'p',
    model: 'm',
    maxContextSize: 1,
    capabilities: ['thinking', 'always_thinking'],
  };

  it('returns the requested effort verbatim when one is provided', () => {
    expect(resolveThinkingEffort('on', { effort: 'medium' }, booleanModel)).toBe('on');
    expect(resolveThinkingEffort('off', { effort: 'medium' }, booleanModel)).toBe('off');
    expect(resolveThinkingEffort('low', { effort: 'medium' }, booleanModel)).toBe('low');
    // No normalization: empty / whitespace strings are returned as-is.
    expect(resolveThinkingEffort('', { enabled: false, effort: 'medium' }, booleanModel)).toBe('');
    expect(resolveThinkingEffort('   ', { enabled: false, effort: 'medium' }, booleanModel)).toBe(
      '   ',
    );
  });

  it('treats config.enabled=false as off when no effort is requested', () => {
    expect(
      resolveThinkingEffort(undefined, { enabled: false, effort: 'medium' }, booleanModel),
    ).toBe('off');
    expect(resolveThinkingEffort(undefined, { enabled: false }, booleanModel)).toBe('off');
  });

  it('uses config.effort as the default effort when enabled', () => {
    expect(resolveThinkingEffort(undefined, { effort: 'medium' }, booleanModel)).toBe('medium');
    expect(resolveThinkingEffort(undefined, { enabled: true, effort: 'medium' }, booleanModel)).toBe(
      'medium',
    );
  });

  it('falls back to the model default effort when no effort is set', () => {
    // boolean thinking model -> 'on'
    expect(resolveThinkingEffort(undefined, {}, booleanModel)).toBe('on');
    // effort-capable model -> middle supportEfforts entry
    expect(resolveThinkingEffort(undefined, {}, effortModel)).toBe('medium');
    // no / non-thinking model -> 'off'
    expect(resolveThinkingEffort(undefined, {}, undefined)).toBe('off');
  });

  it('forces always-thinking models back on even when off is requested', () => {
    expect(resolveThinkingEffort('off', { enabled: false }, alwaysThinkingModel, true)).toBe('on');
    expect(resolveThinkingEffort(undefined, { enabled: false }, alwaysThinkingModel, true)).toBe('on');
  });
});

describe('google base URL forwarding', () => {
  it('forwards base_url to the google-genai provider config', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        defaultModel: 'gemini',
        providers: {
          gemini: {
            type: 'google-genai',
            apiKey: 'g-key',
            baseUrl: 'https://qianxun.example/v1beta',
          },
        },
        models: {
          gemini: { provider: 'gemini', model: 'gemini-2.5-pro', maxContextSize: 1_000_000 },
        },
      },
    });

    expect(resolved.provider).toMatchObject({
      type: 'google-genai',
      model: 'gemini-2.5-pro',
      baseUrl: 'https://qianxun.example/v1beta',
    });
  });

  it('reads GOOGLE_GEMINI_BASE_URL from provider env as a fallback', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        defaultModel: 'gemini',
        providers: {
          gemini: {
            type: 'google-genai',
            apiKey: 'g-key',
            env: { GOOGLE_GEMINI_BASE_URL: 'https://env.example/v1beta' },
          },
        },
        models: {
          gemini: { provider: 'gemini', model: 'gemini-2.5-pro', maxContextSize: 1_000_000 },
        },
      },
    });

    expect(resolved.provider).toMatchObject({
      type: 'google-genai',
      baseUrl: 'https://env.example/v1beta',
    });
  });

  it('forwards a custom proxy base_url to the vertexai provider config', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        defaultModel: 'gemini',
        providers: {
          vertex: {
            type: 'vertexai',
            apiKey: 'v-key',
            baseUrl: 'https://qianxun.example/vertex',
          },
        },
        models: {
          gemini: { provider: 'vertex', model: 'gemini-1.5-pro', maxContextSize: 1_000_000 },
        },
      },
    });

    expect(resolved.provider).toMatchObject({
      type: 'vertexai',
      model: 'gemini-1.5-pro',
      baseUrl: 'https://qianxun.example/vertex',
    });
  });

  it('forwards base_url to vertexai while still deriving location from an aiplatform host', () => {
    // Backward compatibility: an aiplatform host must keep populating `location`
    // (existing GCP behavior) while the base URL is now also forwarded so the
    // SDK targets the configured endpoint verbatim.
    const resolved = resolveRuntimeProvider({
      config: {
        defaultModel: 'gemini',
        providers: {
          vertex: {
            type: 'vertexai',
            apiKey: 'v-key',
            baseUrl: 'https://us-central1-aiplatform.googleapis.com',
          },
        },
        models: {
          gemini: { provider: 'vertex', model: 'gemini-1.5-pro', maxContextSize: 1_000_000 },
        },
      },
    });

    expect(resolved.provider).toMatchObject({
      type: 'vertexai',
      baseUrl: 'https://us-central1-aiplatform.googleapis.com',
      location: 'us-central1',
    });
  });

  it('derives vertex location from the GOOGLE_VERTEX_BASE_URL env fallback so ADC mode is selected', () => {
    // The env fallback must behave exactly like config `base_url`: when the
    // regional endpoint is supplied via GOOGLE_VERTEX_BASE_URL (with a project
    // but no explicit GOOGLE_CLOUD_LOCATION), location derivation must still see
    // it, so the provider resolves to service-account (ADC) mode rather than
    // silently downgrading to API-key Gemini routing.
    const resolved = resolveRuntimeProvider({
      config: {
        defaultModel: 'gemini',
        providers: {
          vertex: {
            type: 'vertexai',
            env: {
              GOOGLE_CLOUD_PROJECT: 'my-proj',
              GOOGLE_VERTEX_BASE_URL: 'https://us-central1-aiplatform.googleapis.com',
            },
          },
        },
        models: {
          gemini: { provider: 'vertex', model: 'gemini-1.5-pro', maxContextSize: 1_000_000 },
        },
      },
    });

    expect(resolved.provider).toMatchObject({
      type: 'vertexai',
      vertexai: true,
      baseUrl: 'https://us-central1-aiplatform.googleapis.com',
      project: 'my-proj',
      location: 'us-central1',
    });
  });
});

describe('per-model protocol routing', () => {
  it('routes a protocol:anthropic model on a kimi provider through the anthropic transport with the REST base stripped of /v1', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        ...BASE_CONFIG,
        models: {
          'mirri-code/kimi-for-coding': {
            ...BASE_CONFIG.models!['mirri-code/kimi-for-coding']!,
            protocol: 'anthropic',
          },
        },
      },
    });

    expect(resolved.providerName).toBe('managed:mirri-code');
    expect(resolved.provider).toMatchObject({
      type: 'anthropic',
      model: 'kimi-for-coding',
      baseUrl: 'https://api.example',
    });
  });

  it('keeps a model without protocol on the provider wire type and leaves the REST base intact', () => {
    const resolved = resolveRuntimeProvider({ config: BASE_CONFIG });

    expect(resolved.provider).toMatchObject({
      type: 'openai',
      model: 'kimi-for-coding',
      baseUrl: 'https://api.example/v1',
    });
  });

  it('does not strip the baseUrl of a provider that is itself typed anthropic', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        defaultModel: 'claude',
        providers: {
          anthropic: {
            type: 'anthropic',
            apiKey: 'sk-anthropic',
            baseUrl: 'https://api.anthropic.example/v1',
          },
        },
        models: {
          claude: {
            provider: 'anthropic',
            model: 'claude-sonnet-4-5',
            maxContextSize: 200_000,
          },
        },
      },
    });

    expect(resolved.provider).toMatchObject({
      type: 'anthropic',
      model: 'claude-sonnet-4-5',
      baseUrl: 'https://api.anthropic.example/v1',
    });
  });
});

describe('resolveRuntimeProvider model overrides', () => {
  it('forwards effective supportEfforts to the openai provider config', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        ...BASE_CONFIG,
        models: {
          'mirri-code/kimi-for-coding': {
            ...BASE_CONFIG.models!['mirri-code/kimi-for-coding']!,
            supportEfforts: ['low', 'high'],
          },
        },
      },
    });

    // supportEfforts must be forwarded so the openai-legacy provider can guard
    // the reasoning_effort "medium" auto-injection (#1616): models that don't
    // advertise "medium" (e.g. Kimi K2.6 with ["on"]) must not receive it.
    expect(resolved.provider).toMatchObject({ type: 'openai' });
    expect(resolved.provider).toHaveProperty('supportEfforts', ['low', 'high']);
  });
});

describe('provider api_key env-var expansion', () => {
  it('should expand ${VAR} in api_key when the env var is set', () => {
    const original = process.env['TEST_PROVIDER_API_KEY'];
    process.env['TEST_PROVIDER_API_KEY'] = 'sk-expanded-real-key';
    try {
      const resolved = resolveRuntimeProvider({
        config: {
          ...BASE_CONFIG,
          providers: {
            'managed:mirri-code': {
              type: 'openai',
              apiKey: '${TEST_PROVIDER_API_KEY}',
              baseUrl: 'https://api.example/v1',
            },
          },
        },
      });
      expect(resolved.provider).toMatchObject({
        type: 'openai',
        apiKey: 'sk-expanded-real-key',
      });
    } finally {
      if (original === undefined) delete process.env['TEST_PROVIDER_API_KEY'];
      else process.env['TEST_PROVIDER_API_KEY'] = original;
    }
  });

  it('should expand ${env:VAR} variant in api_key', () => {
    const original = process.env['TEST_ENV_PREFIX_KEY'];
    process.env['TEST_ENV_PREFIX_KEY'] = 'sk-env-prefix-key';
    try {
      const resolved = resolveRuntimeProvider({
        config: {
          ...BASE_CONFIG,
          providers: {
            'managed:mirri-code': {
              type: 'openai',
              apiKey: '${env:TEST_ENV_PREFIX_KEY}',
              baseUrl: 'https://api.example/v1',
            },
          },
        },
      });
      expect(resolved.provider).toMatchObject({
        apiKey: 'sk-env-prefix-key',
      });
    } finally {
      if (original === undefined) delete process.env['TEST_ENV_PREFIX_KEY'];
      else process.env['TEST_ENV_PREFIX_KEY'] = original;
    }
  });

  it('should fall back to provider.env when the referenced env var is unset', () => {
    const resolved = resolveRuntimeProvider({
      config: {
        ...BASE_CONFIG,
        providers: {
          'managed:mirri-code': {
            type: 'openai',
            apiKey: '${TEST_UNSET_VAR_12345}',
            baseUrl: 'https://api.example/v1',
            env: { OPENAI_API_KEY: 'sk-fallback-from-env-table' },
          },
        },
      },
    });
    // Unset var → empty string → nonEmptyString treats as undefined →
    // falls back to provider.env['OPENAI_API_KEY'].
    expect(resolved.provider).toMatchObject({
      apiKey: 'sk-fallback-from-env-table',
    });
  });
});
