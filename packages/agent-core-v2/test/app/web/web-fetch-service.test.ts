/**
 * `web` domain tests — `WebFetchService` backend selection.
 *
 * Locks in the precedence chain after the managed-login retirement: an
 * explicit `[services.moonshot_fetch]` config section with a `baseUrl` wins
 * (built with its `apiKey`; an `oauth` ref no longer resolves a token), and
 * the built-in local fetcher is the fallback so `FetchURL` keeps working
 * without any configuration.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { DisposableStore } from '#/_base/di/lifecycle';
import { createServices, type TestInstantiationService } from '#/_base/di/test';
import { SERVICES_SECTION, type ServicesConfig } from '#/app/auth/configSection';
import { IBootstrapService } from '#/app/bootstrap/bootstrap';
import { IConfigService } from '#/app/config/config';
import { LocalFetchURLProvider } from '#/app/web/providers/local-fetch-url';
import { MoonshotFetchURLProvider } from '#/app/web/providers/moonshot-fetch-url';
import { IWebFetchService } from '#/app/web/web';
import { WebFetchService } from '#/app/web/webService';

describe('WebFetchService', () => {
  let disposables: DisposableStore;
  let ix: TestInstantiationService;
  let servicesConfig: ServicesConfig | undefined;

  beforeEach(() => {
    disposables = new DisposableStore();
    servicesConfig = undefined;
    ix = createServices(disposables, {
      additionalServices: (reg) => {
        reg.definePartialInstance(IBootstrapService, {
          args: {
            requestHeaders: {
              'User-Agent': 'mirri-code-cli/test',
              'X-Msh-Device-Id': 'device-test',
            },
          },
        });
        reg.definePartialInstance(IConfigService, {
          get: ((domain: string) =>
            domain === SERVICES_SECTION ? servicesConfig : undefined) as IConfigService['get'],
        });
        reg.define(IWebFetchService, WebFetchService);
      },
    });
  });

  afterEach(() => {
    disposables.dispose();
    vi.unstubAllGlobals();
  });

  function fetcher(): ReturnType<IWebFetchService['getUrlFetcher']> {
    return ix.get(IWebFetchService).getUrlFetcher();
  }

  it('yields the local fetcher when no services.moonshot_fetch entry is configured', () => {
    expect(fetcher()).toBeInstanceOf(LocalFetchURLProvider);
  });

  it('builds a Moonshot fetcher from the services.moonshot_fetch api_key config', async () => {
    servicesConfig = {
      moonshotFetch: {
        baseUrl: 'https://fetch.example.com/fetch',
        apiKey: 'fetch-key',
        customHeaders: { 'X-Config': '1' },
      },
    };
    const fetchMock = vi.fn().mockResolvedValue({
      status: 200,
      text: async () => 'page body',
    });
    vi.stubGlobal('fetch', fetchMock);

    expect(fetcher()).toBeInstanceOf(MoonshotFetchURLProvider);
    const result = await fetcher().fetch('https://example.com/page');

    expect(result).toEqual({ content: 'page body', kind: 'extracted' });
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe('https://fetch.example.com/fetch');
    const headers = init.headers as Record<string, string>;
    expect(headers['Authorization']).toBe('Bearer fetch-key');
    expect(headers['User-Agent']).toBe('mirri-code-cli/test');
    expect(headers['X-Msh-Device-Id']).toBe('device-test');
    expect(headers['X-Config']).toBe('1');
  });

  it('builds a Moonshot fetcher from a services.moonshot_fetch entry carrying only an oauth ref', () => {
    // Managed-login OAuth is retired: the ref no longer resolves a token
    // source, but the explicit config still wins the precedence chain and the
    // provider is assembled (requests fail at the endpoint, then the local
    // fetcher takes over).
    servicesConfig = {
      moonshotFetch: {
        baseUrl: 'https://fetch.example.com/fetch',
        oauth: { storage: 'file', key: 'oauth/mirri-code' },
      },
    };
    expect(fetcher()).toBeInstanceOf(MoonshotFetchURLProvider);
  });

  it('yields the local fetcher when services.moonshot_fetch has no baseUrl', () => {
    servicesConfig = { moonshotFetch: { apiKey: 'fetch-key' } };
    expect(fetcher()).toBeInstanceOf(LocalFetchURLProvider);
  });
});