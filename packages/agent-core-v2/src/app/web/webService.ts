/**
 * `web` domain — `IWebFetchService` implementation.
 *
 * Yields the `UrlFetcher` the `FetchURL` tool uses, resolving the backend in
 * precedence order: (1) an explicit `[services.moonshot_fetch]` config section
 * with a `baseUrl` — built with its `apiKey` (managed-login OAuth bearer was
 * retired with the managed auth domain, so an `oauth` ref yields no token
 * source); and (2) the built-in `LocalFetchURLProvider`, so `FetchURL` keeps
 * working without any configuration. The explicit config uses the host's
 * identity headers (`IBootstrapService.args.requestHeaders`) and falls back to
 * the local fetcher on failure. Reads config lazily on each `getUrlFetcher()`
 * call so it tracks edits. Bound at App scope.
 */

import { LifecycleScope, ScopeActivation, registerScopedService } from '#/_base/di/scope';
import { SERVICES_SECTION, type ServicesConfig } from '#/app/auth/configSection';
import { IBootstrapService } from '#/app/bootstrap/bootstrap';
import { IConfigService } from '#/app/config/config';

import { LocalFetchURLProvider } from './providers/local-fetch-url';
import { MoonshotFetchURLProvider } from './providers/moonshot-fetch-url';
import type { UrlFetcher } from './tools/fetch-url-types';
import { IWebFetchService } from './web';

export class WebFetchService implements IWebFetchService {
  declare readonly _serviceBrand: undefined;
  private readonly localFetcher: UrlFetcher;

  constructor(
    @IBootstrapService private readonly bootstrap: IBootstrapService,
    @IConfigService private readonly config: IConfigService,
  ) {
    this.localFetcher = new LocalFetchURLProvider();
  }

  getUrlFetcher(): UrlFetcher {
    return this.fromServicesConfig() ?? this.localFetcher;
  }

  private fromServicesConfig(): UrlFetcher | undefined {
    const fetchConfig = this.config.get<ServicesConfig>(SERVICES_SECTION)?.moonshotFetch;
    if (fetchConfig?.baseUrl === undefined) {
      return undefined;
    }
    return new MoonshotFetchURLProvider({
      baseUrl: fetchConfig.baseUrl,
      apiKey: nonEmptyString(fetchConfig.apiKey),
      defaultHeaders: { ...this.bootstrap.args.requestHeaders },
      customHeaders: fetchConfig.customHeaders,
      localFallback: this.localFetcher,
    });
  }
}

function nonEmptyString(value: string | undefined): string | undefined {
  const trimmed = value?.trim();
  return trimmed === undefined || trimmed.length === 0 ? undefined : trimmed;
}

registerScopedService(
  LifecycleScope.App,
  IWebFetchService,
  WebFetchService,
  ScopeActivation.OnScopeCreated,
  'web',
);