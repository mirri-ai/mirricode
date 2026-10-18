/**
 * `auth` domain (cross-cutting) — `IWebSearchProviderService` implementation.
 *
 * Resolves the `WebSearch` backend from the explicit
 * `[services.moonshot_search]` config section (read through `config`), built
 * with its `apiKey` (the managed Kimi OAuth branch was retired with the
 * managed auth domain — an `oauth` ref yields no token source). Uses the
 * host's identity headers (`IBootstrapService.args.requestHeaders`) as
 * default headers. When no source is configured it yields `undefined`.
 * Tests and hosts that need a custom backend bind `IWebSearchProviderService`
 * directly. Bound at App scope.
 */

import { LifecycleScope, ScopeActivation, registerScopedService } from '#/_base/di/scope';
import { IBootstrapService } from '#/app/bootstrap/bootstrap';
import { IConfigService } from '#/app/config/config';

import { SERVICES_SECTION, type ServicesConfig } from '../configSection';
import { MoonshotWebSearchProvider } from './providers/moonshot-web-search';
import type { WebSearchProvider } from '#/agent/tools/web-search/web-search';
import { IWebSearchProviderService } from './webSearch';

export class WebSearchProviderService implements IWebSearchProviderService {
  declare readonly _serviceBrand: undefined;

  constructor(
    @IBootstrapService private readonly bootstrap: IBootstrapService,
    @IConfigService private readonly config: IConfigService,
  ) {}

  getWebSearchProvider(): WebSearchProvider | undefined {
    return this.fromServicesConfig();
  }

  private fromServicesConfig(): WebSearchProvider | undefined {
    const search = this.config.get<ServicesConfig>(SERVICES_SECTION)?.moonshotSearch;
    if (search?.baseUrl === undefined) {
      return undefined;
    }
    return new MoonshotWebSearchProvider({
      baseUrl: search.baseUrl,
      apiKey: nonEmptyString(search.apiKey),
      defaultHeaders: { ...this.bootstrap.args.requestHeaders },
      customHeaders: search.customHeaders,
    });
  }
}

function nonEmptyString(value: string | undefined): string | undefined {
  const trimmed = value?.trim();
  return trimmed === undefined || trimmed.length === 0 ? undefined : trimmed;
}

registerScopedService(
  LifecycleScope.App,
  IWebSearchProviderService,
  WebSearchProviderService,
  ScopeActivation.OnScopeCreated,
  'auth',
);