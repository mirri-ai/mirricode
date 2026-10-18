import { APIStatusError } from '@mirri-ai/kosong';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { ErrorCodes, MirriForCodingProvider } from '#/index';

import { TEST_IDENTITY } from './test-identity';

describe('MirriForCodingProvider', () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  function createProvider(overrides: { readonly apiKey?: string; readonly homeDir?: string } = {}) {
    return new MirriForCodingProvider({
      homeDir: overrides.homeDir ?? '/tmp',
      ...TEST_IDENTITY,
      apiKey: overrides.apiKey,
    });
  }

  it('serves the configured API key to the request at call time', async () => {
    const auth = createProvider({ apiKey: 'sk-mirri-123' }).resolveAuth('kimi-for-coding');

    await expect(
      auth(async (auth) => {
        expect(auth).toEqual({ apiKey: 'sk-mirri-123' });
        return 'ok';
      }),
    ).resolves.toBe('ok');
  });

  it('rejects the request with auth.login_required when no API key is configured', async () => {
    const auth = createProvider({}).resolveAuth('kimi-for-coding');

    await expect(auth(async () => 'unreachable')).rejects.toMatchObject({
      code: ErrorCodes.AUTH_LOGIN_REQUIRED,
    });
  });

  it('maps a 401 from the upstream to auth.login_required', async () => {
    const auth = createProvider({ apiKey: 'rejected-key' }).resolveAuth('kimi-for-coding');
    const upstreamError = new APIStatusError(401, 'Unauthorized');

    await expect(auth(async () => { throw upstreamError; })).rejects.toMatchObject({
      code: ErrorCodes.AUTH_LOGIN_REQUIRED,
      cause: upstreamError,
    });
  });

  it('passes non-401 upstream errors through raw', async () => {
    const auth = createProvider({ apiKey: 'sk-mirri-123' }).resolveAuth('kimi-for-coding');
    const upstreamError = new Error('upstream exploded');

    await expect(auth(async () => { throw upstreamError; })).rejects.toBe(upstreamError);
  });

  it('rejects an unsupported model at provider config resolution time', () => {
    const provider = createProvider({ apiKey: 'sk-mirri-123' });

    expect(() => provider.resolveProviderConfig('other-model')).toThrow(
      /not supported by MirriForCodingProvider/,
    );
  });

  it('builds resolved provider config with host identity headers', () => {
    const provider = createProvider({ apiKey: 'sk-mirri-123' });
    const resolved = provider.resolveProviderConfig('kimi-for-coding');

    expect(provider.defaultModel).toBe('kimi-for-coding');
    expect(resolved.provider).toMatchObject({
      type: 'openai',
      model: 'kimi-for-coding',
      baseUrl: 'https://api.kimi.com/coding/v1',
    });
    const headers = (resolved.provider as { defaultHeaders?: Record<string, string> }).defaultHeaders;
    expect(headers?.['X-Msh-Version']).toBe('0.0.0-test');
    expect(headers?.['X-Msh-Platform']).toBeDefined();
  });
});