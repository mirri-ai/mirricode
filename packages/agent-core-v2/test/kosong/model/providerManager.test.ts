/**
 * B0-L19: Provider Manager port tests.
 *
 * Standalone unit tests for the three aspects of the provider-manager port
 * onto v2's inlined kosong:
 *
 *  1. `resolveOutboundHeaders` — env custom headers, host-header forwarding
 *     (full identity vs User-Agent-only), customHeaders override.
 *  2. `resolveModelCapabilities` — declared + detected capability merge,
 *     including `dynamically_loaded_tools`.
 *  3. `AuthLegacyService` — managed_provider summary returns
 *     `{ name, status: 'unauthenticated' }` when the provider is configured
 *     but not logged in (never null).
 */

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { MIRRICODE_PROVIDER_NAME } from '@mirri-ai/v2-oauth';

import { DisposableStore } from '#/_base/di/lifecycle';
import { createServices, type TestInstantiationService } from '#/_base/di/test';
import { IAuthLegacyService } from '#/app/authLegacy/authLegacy';
import { AuthLegacyService } from '#/app/authLegacy/authLegacyService';
import { IOAuthService } from '#/app/auth/auth';
import { IProviderService, type ProviderConfig } from '#/kosong/provider/provider';
import { IModelService } from '#/kosong/model/model';
import { resolveOutboundHeaders } from '#/kosong/model/catalogService';
import { ILogService } from '#/_base/log/log';
import { IConfigService } from '#/app/config/config';

// ---------------------------------------------------------------------------
// Side-effect registration: provider definitions (kimi = hostHeaders: 'full')
// ---------------------------------------------------------------------------
import '#/kosong/provider/providers/kimi/kimi.contrib';
import '#/kosong/provider/providers/standard.contrib';
import '#/kosong/provider/bases/openai/index';
import '#/kosong/provider/bases/anthropic/index';

import { registerBootstrapServices } from '../../app/bootstrap/stubs';
import { registerTelemetryServices } from '../../app/telemetry/stubs';

// ---------------------------------------------------------------------------
// 1. resolveOutboundHeaders
// ---------------------------------------------------------------------------

describe('resolveOutboundHeaders', () => {

  afterEach(() => {
    // Clean up env so MIRRICODE_CUSTOM_HEADERS doesn't leak between tests.
    delete process.env['MIRRICODE_CUSTOM_HEADERS'];
  });

  it('should forward full host headers when provider type declares hostHeaders: full', () => {
    const hostHeaders = {
      'User-Agent': 'mirri-code/1.0',
      'X-Msh-Platform': 'kimi_code_cli',
      'X-Msh-Device-Id': 'device-abc',
    };
    const result = resolveOutboundHeaders('kimi', undefined, hostHeaders);
    expect(result).toMatchObject({
      'User-Agent': 'mirri-code/1.0',
      'X-Msh-Platform': 'kimi_code_cli',
      'X-Msh-Device-Id': 'device-abc',
    });
  });

  it('should forward only User-Agent when provider type does not declare hostHeaders: full', () => {
    const hostHeaders = {
      'User-Agent': 'mirri-code/1.0',
      'X-Msh-Platform': 'kimi_code_cli',
      'X-Msh-Device-Id': 'device-abc',
    };
    const result = resolveOutboundHeaders('openai', undefined, hostHeaders);
    expect(result).toEqual({ 'User-Agent': 'mirri-code/1.0' });
    // Device identity headers must NOT leak to third-party endpoints.
    expect(result).not.toHaveProperty('X-Msh-Device-Id');
    expect(result).not.toHaveProperty('X-Msh-Platform');
  });

  it('should forward only User-Agent for anthropic provider type', () => {
    const hostHeaders = {
      'User-Agent': 'mirri-code/1.0',
      'X-Msh-Device-Id': 'device-xyz',
    };
    const result = resolveOutboundHeaders('anthropic', undefined, hostHeaders);
    expect(result).toEqual({ 'User-Agent': 'mirri-code/1.0' });
  });

  it('should forward only User-Agent when provider type is undefined', () => {
    const hostHeaders = {
      'User-Agent': 'mirri-code/1.0',
      'X-Msh-Device-Id': 'device-xyz',
    };
    const result = resolveOutboundHeaders(undefined, undefined, hostHeaders);
    expect(result).toEqual({ 'User-Agent': 'mirri-code/1.0' });
  });

  it('should return empty headers when no User-Agent is in host headers and provider is not full', () => {
    const hostHeaders = { 'X-Msh-Device-Id': 'device-xyz' };
    const result = resolveOutboundHeaders('openai', undefined, hostHeaders);
    // No User-Agent → nothing to forward for non-full vendors.
    expect(result).toEqual({});
  });

  it('should merge env custom headers as lowest precedence', () => {
    process.env['MIRRICODE_CUSTOM_HEADERS'] = 'X-Custom: from-env\nX-Other: other-value';
    const hostHeaders = { 'User-Agent': 'mirri-code/1.0' };
    const result = resolveOutboundHeaders('openai', undefined, hostHeaders);
    expect(result).toMatchObject({
      'X-Custom': 'from-env',
      'X-Other': 'other-value',
      'User-Agent': 'mirri-code/1.0',
    });
  });

  it('should let host headers override env custom headers', () => {
    process.env['MIRRICODE_CUSTOM_HEADERS'] = 'User-Agent: env-ua/0.1';
    const hostHeaders = { 'User-Agent': 'mirri-code/1.0' };
    const result = resolveOutboundHeaders('openai', undefined, hostHeaders);
    expect(result['User-Agent']).toBe('mirri-code/1.0');
  });

  it('should let customHeaders override both env and host headers', () => {
    process.env['MIRRICODE_CUSTOM_HEADERS'] = 'X-Base: from-env';
    const hostHeaders = { 'User-Agent': 'mirri-code/1.0', 'X-Host': 'from-host' };
    const customHeaders = { 'X-Host': 'from-custom', 'X-Custom': 'from-custom' };
    const result = resolveOutboundHeaders('kimi', customHeaders, hostHeaders);
    expect(result['X-Host']).toBe('from-custom');
    expect(result['X-Custom']).toBe('from-custom');
    expect(result['X-Base']).toBe('from-env');
    expect(result['User-Agent']).toBe('mirri-code/1.0');
  });

  it('should handle empty MIRRICODE_CUSTOM_HEADERS gracefully', () => {
    process.env['MIRRICODE_CUSTOM_HEADERS'] = '';
    const hostHeaders = { 'User-Agent': 'mirri-code/1.0' };
    const result = resolveOutboundHeaders('openai', undefined, hostHeaders);
    expect(result).toEqual({ 'User-Agent': 'mirri-code/1.0' });
  });

  it('should skip malformed lines in MIRRICODE_CUSTOM_HEADERS', () => {
    process.env['MIRRICODE_CUSTOM_HEADERS'] = 'X-Valid: ok\nno-colon-here\n : empty-name\nX-Also-Valid: yes';
    const hostHeaders = { 'User-Agent': 'mirri-code/1.0' };
    const result = resolveOutboundHeaders('openai', undefined, hostHeaders);
    expect(result).toMatchObject({
      'X-Valid': 'ok',
      'X-Also-Valid': 'yes',
      'User-Agent': 'mirri-code/1.0',
    });
    expect(Object.keys(result)).toHaveLength(3);
  });
});

// ---------------------------------------------------------------------------
// 2. AuthLegacyService — managed_provider summary
// ---------------------------------------------------------------------------

describe('AuthLegacyService managed_provider summary', () => {
  const OAUTH_PROVIDER = MIRRICODE_PROVIDER_NAME; // 'managed:mirri-code'
  const NON_OAUTH_PROVIDER = 'openai-main';

  let disposables: DisposableStore;
  let ix: TestInstantiationService;
  let providers: Record<string, ProviderConfig>;
  let defaultModel: string | undefined;
  let oauthStatus: ReturnType<typeof vi.fn>;
  let providerReadyResolve: () => void;
  let providerReady: Promise<void>;

  beforeEach(() => {
    disposables = new DisposableStore();
    providers = {};
    defaultModel = undefined;
    oauthStatus = vi.fn().mockResolvedValue({ loggedIn: false });
    providerReady = new Promise<void>((resolve) => {
      providerReadyResolve = resolve;
    });
    // Resolve immediately by default so existing tests aren't affected.
    providerReadyResolve();

    ix = createServices(disposables, {
      base: [registerBootstrapServices, registerTelemetryServices],
      additionalServices: (reg) => {
        reg.definePartialInstance(IProviderService, {
          get: ((name: string) => providers[name]) as IProviderService['get'],
          list: (() => providers) as IProviderService['list'],
          getDefaultProvider: (() => undefined) as IProviderService['getDefaultProvider'],
          ready: providerReady as IProviderService['ready'],
        });
        reg.definePartialInstance(IModelService, {
          getDefaultModel: (() => defaultModel) as IModelService['getDefaultModel'],
          ready: Promise.resolve() as IModelService['ready'],
        });
        reg.definePartialInstance(IConfigService, {
          get: ((domain: string) =>
            domain === 'defaultModel' ? defaultModel : undefined) as IConfigService['get'],
        });
        reg.definePartialInstance(IOAuthService, {
          status: oauthStatus as unknown as IOAuthService['status'],
        });
        reg.definePartialInstance(ILogService, {
          info: vi.fn(),
          warn: vi.fn(),
          debug: vi.fn(),
          error: vi.fn(),
        });
        reg.define(IAuthLegacyService, AuthLegacyService);
      },
    });
  });

  afterEach(() => disposables.dispose());

  function createService(): IAuthLegacyService {
    return ix.get(IAuthLegacyService);
  }

  it('should return null managed_provider when no providers are configured', async () => {
    const summary = await createService().get();
    expect(summary.managed_provider).toBeNull();
  });

  it('should return null managed_provider when only non-oauth providers exist', async () => {
    providers = { [NON_OAUTH_PROVIDER]: { type: 'openai', apiKey: 'sk-test' } };
    defaultModel = 'gpt-5';
    const summary = await createService().get();
    expect(summary.managed_provider).toBeNull();
  });

  it('should return { name, status: "unauthenticated" } when managed provider is configured but not logged in', async () => {
    providers = {
      [OAUTH_PROVIDER]: { type: 'kimi', oauth: { storage: 'file', key: 'oauth/mirri-code' } },
    };
    oauthStatus.mockResolvedValue({ loggedIn: false });
    const summary = await createService().get();
    expect(summary.managed_provider).toEqual({
      name: OAUTH_PROVIDER,
      status: 'unauthenticated',
    });
    // Must NOT be null — the provider IS configured.
    expect(summary.managed_provider).not.toBeNull();
  });

  it('should return { name, status: "authenticated" } when managed provider has a cached token', async () => {
    providers = {
      [OAUTH_PROVIDER]: { type: 'kimi', oauth: { storage: 'file', key: 'oauth/mirri-code' } },
    };
    defaultModel = 'k2';
    oauthStatus.mockResolvedValue({ loggedIn: true, provider: OAUTH_PROVIDER });
    const summary = await createService().get();
    expect(summary.managed_provider).toEqual({
      name: OAUTH_PROVIDER,
      status: 'authenticated',
    });
    expect(summary.ready).toBe(true);
  });

  it('should return { name, status: "unauthenticated" } when oauth status check throws', async () => {
    providers = {
      [OAUTH_PROVIDER]: { type: 'kimi', oauth: { storage: 'file', key: 'oauth/mirri-code' } },
    };
    oauthStatus.mockRejectedValue(new Error('token storage unavailable'));
    const summary = await createService().get();
    // On error, managedLoggedIn() returns false → unauthenticated, never null.
    expect(summary.managed_provider).toEqual({
      name: OAUTH_PROVIDER,
      status: 'unauthenticated',
    });
  });

  it('should mark ready=false when managed provider is unauthenticated and no default model', async () => {
    providers = {
      [OAUTH_PROVIDER]: { type: 'kimi', oauth: { storage: 'file', key: 'oauth/mirri-code' } },
    };
    oauthStatus.mockResolvedValue({ loggedIn: false });
    const summary = await createService().get();
    expect(summary.ready).toBe(false);
    expect(summary.managed_provider).toEqual({
      name: OAUTH_PROVIDER,
      status: 'unauthenticated',
    });
  });

  it('should await providerService.ready before reading providers — never returns stale null', async () => {
    // Create a providerReady that has NOT resolved yet.
    let resolveReady!: () => void;
    const pendingReady = new Promise<void>((resolve) => {
      resolveReady = resolve;
    });

    // Re-create the service with a pending provider ready.
    const pendingIx = createServices(disposables, {
      base: [registerBootstrapServices, registerTelemetryServices],
      additionalServices: (reg) => {
        // Providers start EMPTY but will be populated once ready resolves.
        let pendingProviders: Record<string, ProviderConfig> = {};
        reg.definePartialInstance(IProviderService, {
          get: ((name: string) => pendingProviders[name]) as IProviderService['get'],
          list: (() => pendingProviders) as IProviderService['list'],
          getDefaultProvider: (() => undefined) as IProviderService['getDefaultProvider'],
          ready: pendingReady as IProviderService['ready'],
        });
        reg.definePartialInstance(IModelService, {
          getDefaultModel: (() => 'k2') as IModelService['getDefaultModel'],
          ready: Promise.resolve() as IModelService['ready'],
        });
        reg.definePartialInstance(IConfigService, {
          get: ((domain: string) =>
            domain === 'defaultModel' ? 'k2' : undefined) as IConfigService['get'],
        });
        reg.definePartialInstance(IOAuthService, {
          status: vi.fn().mockResolvedValue({ loggedIn: false }) as unknown as IOAuthService['status'],
        });
        reg.definePartialInstance(ILogService, {
          info: vi.fn(),
          warn: vi.fn(),
          debug: vi.fn(),
          error: vi.fn(),
        });
        reg.define(IAuthLegacyService, AuthLegacyService);

        // After the service is created, simulate the hydration that happens
        // when providerService.ready resolves: populate providers, then resolve.
        // Use setTimeout to ensure the get() call has started awaiting first.
        setTimeout(() => {
          pendingProviders = {
            [OAUTH_PROVIDER]: { type: 'kimi', oauth: { storage: 'file', key: 'oauth/mirri-code' } },
          };
          resolveReady();
        }, 10);
      },
    });

    const service = pendingIx.get(IAuthLegacyService);
    const summary = await service.get();

    // Before the fix (not awaiting providerService.ready), the list() call
    // would return {} → managed_provider would be null. After the fix,
    // get() waits for providerService.ready, so list() returns the
    // populated providers, and managed_provider is non-null.
    expect(summary.managed_provider).toEqual({
      name: OAUTH_PROVIDER,
      status: 'unauthenticated',
    });
    expect(summary.providers_count).toBe(1);
  });
});
