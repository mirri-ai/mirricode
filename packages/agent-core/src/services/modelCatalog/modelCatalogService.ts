import { Disposable, InstantiationType, registerSingleton } from '../../di';
import type { MirriConfig, ModelAlias, ProviderConfig, ProviderType } from '../../config';
import type {
  CatalogProvider,
  ModelCatalogItem,
  ProviderCatalogItem,
  RefreshOAuthProviderModelsResponse,
  RefreshProviderModelsResponse,
  SetDefaultModelResponse,
} from '@mirri-ai/protocol';
import {
  catalogProviderModels,
  fetchCatalog,
  inferWireType,
  loadBuiltInCatalog,
  DEFAULT_CATALOG_URL,
  type Catalog,
  type CatalogModel,
  type CatalogProviderEntry,
} from '@mirri-ai/kosong';
import {
  refreshProviderModels,
  type ManagedMirriOAuthRef,
  type RefreshProviderHost,
  type RefreshResult,
} from '@mirri-ai/mirri-code-oauth';

import { createManagedAuthFacade, type ServicesAuthFacade } from '../auth/managedAuth';
import { ICoreProcessService } from '../coreProcess/coreProcess';
import { IEnvironmentService } from '../environment/environment';
import { IEventService } from '../event/event';
import {
  IModelCatalogService,
  CatalogUnavailableError,
  ModelNotFoundError,
  ProviderNotFoundError,
  toProtocolModel,
  toProtocolProvider,
  type AddProviderInput,
  type RefreshProviderModelsOptions,
} from './modelCatalog';

export class ModelCatalogService
  extends Disposable
  implements IModelCatalogService {
  readonly _serviceBrand: undefined;

  private _authFacade: ServicesAuthFacade;

  /** Serializes refresh runs so a scheduled refresh and a manual one (or two
   *  manual ones with different options) never race on writing config.toml. */
  private _refreshChain: Promise<unknown> = Promise.resolve();

  constructor(
    @IEnvironmentService env: IEnvironmentService,
    @ICoreProcessService private readonly core: ICoreProcessService,
    @IEventService private readonly eventService: IEventService,
  ) {
    super();
    this._authFacade = createManagedAuthFacade(env);
  }

  static _createForTest(
    env: IEnvironmentService,
    core: ICoreProcessService,
    authFacade: ServicesAuthFacade,
    eventService: IEventService = noopEventService,
  ): ModelCatalogService {
    const service = new ModelCatalogService(env, core, eventService);
    service._authFacade = authFacade;
    return service;
  }

  async listModels(): Promise<readonly ModelCatalogItem[]> {
    const config = await this._readConfig();
    return Object.entries(config.models ?? {}).map(([modelId, alias]) =>
      toProtocolModel(modelId, alias, this._providerTypeOf(config, alias)),
    );
  }

  async listProviders(): Promise<readonly ProviderCatalogItem[]> {
    const config = await this._readConfig();
    const out: ProviderCatalogItem[] = [];
    for (const [providerId, provider] of Object.entries(config.providers ?? {})) {
      out.push(await this._provider(config, providerId, provider));
    }
    return out;
  }

  async getProvider(providerId: string): Promise<ProviderCatalogItem> {
    const config = await this._readConfig();
    const provider = config.providers?.[providerId];
    if (provider === undefined) {
      throw new ProviderNotFoundError(providerId);
    }
    return this._provider(config, providerId, provider);
  }

  async setDefaultModel(modelId: string): Promise<SetDefaultModelResponse> {
    const config = await this._readConfig();
    const alias = config.models?.[modelId];
    if (alias === undefined) {
      throw new ModelNotFoundError(modelId);
    }

    const updated = await this.core.rpc.setMirriConfig({ defaultModel: modelId });
    const updatedAlias = updated.models?.[modelId] ?? alias;
    return {
      default_model: modelId,
      model: toProtocolModel(
        modelId,
        updatedAlias,
        this._providerTypeOf(updated, updatedAlias),
      ),
    };
  }

  async listCatalogProviders(): Promise<readonly CatalogProvider[]> {
    const catalog = await this._loadCatalog();
    const items: CatalogProvider[] = [];
    for (const [id, entry] of Object.entries(catalog)) {
      if (typeof entry !== 'object' || entry === null) continue;
      const wire = inferWireType(entry);
      const models = catalogProviderModels(entry);
      items.push(toCatalogProvider(id, entry, wire, models));
    }
    return items;
  }

  async addProvider(input: AddProviderInput): Promise<ProviderCatalogItem> {
    const patch: Record<string, unknown> = {
      providers: {
        [input.type]: {
          type: input.type,
          apiKey: input.apiKey,
          baseUrl: input.baseUrl,
        },
      },
    };

    // If this provider id exists in the remote catalog, import its models
    // automatically (mirrors the TUI /provider flow: picking a catalog provider
    // writes all its model aliases into config in one shot).
    try {
      const catalog = await this._loadCatalog();
      const entry = catalog[input.type];
      if (entry !== undefined) {
        const wire = inferWireType(entry);
        const models = catalogProviderModels(entry);
        if (models.length > 0) {
          const modelsPatch: Record<string, ModelAlias> = {};
          for (const model of models) {
            const key = `${input.type}/${model.id}`;
            modelsPatch[key] = {
              provider: input.type,
              model: model.id,
              maxContextSize: model.capability.max_context_tokens,
              maxOutputSize: model.maxOutputSize,
              capabilities: catalogModelToCapabilityStrings(model),
              displayName: model.name,
              reasoningKey: model.reasoningKey,
            };
          }
          // Override the provider type with the catalog-inferred wire type
          // when available — e.g. selecting "anthropic" should write type
          // "anthropic", not the raw catalog id.
          if (wire !== undefined) {
            const providerPatch = patch['providers'] as Record<string, Record<string, unknown>>;
            providerPatch[input.type]!['type'] = wire;
          }
          patch['models'] = modelsPatch;
        }
      }
    } catch {
      // Catalog unavailable — provider is created without auto-imported
      // models; the user can add models manually or run a refresh afterwards.
    }

    if (input.defaultModel !== undefined) {
      patch['defaultModel'] = input.defaultModel;
    }
    const updated = await this.core.rpc.setMirriConfig(patch);
    const provider = updated.providers[input.type];
    if (provider === undefined) {
      throw new ProviderNotFoundError(input.type);
    }
    return this._provider(updated, input.type, provider);
  }

  async removeProvider(providerId: string): Promise<void> {
    const config = await this._readConfig();
    if (config.providers?.[providerId] === undefined) {
      throw new ProviderNotFoundError(providerId);
    }
    await this.core.rpc.removeMirriProvider({ providerId });
  }

  async removeModel(modelId: string): Promise<void> {
    const config = await this._readConfig();
    if (config.models?.[modelId] === undefined) {
      throw new ModelNotFoundError(modelId);
    }
    await this.core.rpc.removeMirriModel({ modelId });
  }

  private _providerTypeOf(config: MirriConfig, alias: ModelAlias): ProviderType | undefined {
    const providerId = alias.provider ?? config.defaultProvider;
    return config.providers[providerId ?? '']?.type;
  }

  private async _loadCatalog(): Promise<Catalog> {
    try {
      return await fetchCatalog(DEFAULT_CATALOG_URL);
    } catch {
      // Fall back to built-in catalog injected at build time
      const builtIn = loadBuiltInCatalog(
        __MIRRICODE_BUILT_IN_CATALOG__ !== undefined
          ? __MIRRICODE_BUILT_IN_CATALOG__
          : undefined,
      );
      if (builtIn !== undefined) return builtIn;
      throw new CatalogUnavailableError();
    }
  }

  async refreshOAuthProviderModels(): Promise<RefreshOAuthProviderModelsResponse> {
    return this.refreshProviderModels({ scope: 'oauth' });
  }

  refreshProviderModels(
    options: RefreshProviderModelsOptions = {},
  ): Promise<RefreshProviderModelsResponse> {
    const run = this._refreshChain.then(() => this._doRefreshProviderModels(options));
    this._refreshChain = run.then(
      () => undefined,
      () => undefined,
    );
    return run;
  }

  private async _doRefreshProviderModels(
    options: RefreshProviderModelsOptions,
  ): Promise<RefreshProviderModelsResponse> {
    if (options.providerId !== undefined) {
      const config = await this._readConfig();
      if (config.providers?.[options.providerId] === undefined) {
        throw new ProviderNotFoundError(options.providerId);
      }
    }

    const result = await refreshProviderModels(this._buildRefreshHost(), {
      scope: options.scope,
      providerId: options.providerId,
    });
    const response = mapRefreshResult(result);

    if (response.changed.length > 0) {
      this.eventService.publish({
        type: 'event.model_catalog.changed',
        agentId: 'main',
        sessionId: '__global__',
        changed: response.changed,
        unchanged: response.unchanged,
        failed: response.failed,
      });
    }

    return response;
  }

  private _buildRefreshHost(): RefreshProviderHost {
    return {
      getConfig: () => this._readConfig(),
      removeProvider: (providerId) => this.core.rpc.removeMirriProvider({ providerId }),
      setConfig: (patch) => this.core.rpc.setMirriConfig(patch as Record<string, unknown>),
      resolveOAuthToken: (providerName, oauthRef) =>
        this._resolveOAuthToken(providerName, oauthRef),
    };
  }

  private async _resolveOAuthToken(
    providerName: string,
    oauthRef?: ManagedMirriOAuthRef,
  ): Promise<string> {
    const tokenProvider = this._authFacade.resolveOAuthTokenProvider(providerName, oauthRef);
    if (tokenProvider === undefined) {
      throw new Error('OAuth token provider is not configured.');
    }
    return tokenProvider.getAccessToken();
  }

  private async _readConfig(): Promise<MirriConfig> {
    return this.core.rpc.getMirriConfig({ reload: true });
  }

  private async _provider(
    config: MirriConfig,
    providerId: string,
    provider: ProviderConfig,
  ): Promise<ProviderCatalogItem> {
    const hasApiKey = hasConfiguredApiKey(provider);
    const hasOAuthToken = await this._hasCachedToken(providerId, provider);
    return toProtocolProvider(providerId, provider, config, {
      hasApiKey,
      hasOAuthToken,
    });
  }

  private async _hasCachedToken(
    providerId: string,
    provider: ProviderConfig,
  ): Promise<boolean> {
    if (provider.oauth === undefined) return false;
    try {
      const token = await this._authFacade.getCachedAccessToken(
        providerId,
        provider.oauth,
      );
      return nonEmpty(token) !== undefined;
    } catch {
      return false;
    }
  }
}

function mapRefreshResult(result: RefreshResult): RefreshProviderModelsResponse {
  return {
    changed: result.changed.map((change) => ({
      provider_id: change.providerId,
      provider_name: change.providerName,
      added: change.added,
      removed: change.removed,
    })),
    unchanged: [...result.unchanged],
    failed: result.failed.map((failure) => ({
      provider: failure.provider,
      reason: failure.reason,
    })),
  };
}

function hasConfiguredApiKey(provider: ProviderConfig): boolean {
  if (nonEmpty(provider.apiKey) !== undefined) return true;
  switch (provider.type) {
    case 'anthropic':
      return nonEmpty(provider.env?.['ANTHROPIC_API_KEY']) !== undefined;
    case 'openai':
    case 'openai_responses':
      return nonEmpty(provider.env?.['OPENAI_API_KEY']) !== undefined;
    case 'google-genai':
      return nonEmpty(provider.env?.['GOOGLE_API_KEY']) !== undefined;
    case 'vertexai':
      return (
        nonEmpty(provider.env?.['VERTEXAI_API_KEY']) !== undefined ||
        nonEmpty(provider.env?.['GOOGLE_API_KEY']) !== undefined
      );
  }
  return false;
}

function nonEmpty(value: string | undefined): string | undefined {
  if (value === undefined) return undefined;
  const trimmed = value.trim();
  return trimmed.length === 0 ? undefined : trimmed;
}

/** Converts a CatalogModel's capability flags to the string array used in
 *  ModelAlias.capabilities (image_in, thinking, tool_use, etc.). */
function catalogModelToCapabilityStrings(model: CatalogModel): string[] | undefined {
  const caps: string[] = [];
  if (model.capability.image_in) caps.push('image_in');
  if (model.capability.video_in) caps.push('video_in');
  if (model.capability.audio_in) caps.push('audio_in');
  if (model.capability.thinking) caps.push('thinking');
  if (model.capability.tool_use) caps.push('tool_use');
  if (model.capability.dynamically_loaded_tools === true) caps.push('dynamically_loaded_tools');
  return caps.length > 0 ? caps : undefined;
}

const noopEventService: IEventService = {
  _serviceBrand: undefined,
  onDidPublish: () => ({ dispose: () => undefined }),
  publish: () => undefined,
};

registerSingleton(IModelCatalogService, ModelCatalogService, InstantiationType.Delayed);

declare const __MIRRICODE_BUILT_IN_CATALOG__: string | undefined;

function toCatalogProvider(
  id: string,
  entry: CatalogProviderEntry,
  wire: ProviderType | undefined,
  models: CatalogModel[],
): CatalogProvider {
  return {
    id,
    name: entry.name,
    api: entry.api,
    npm: entry.npm,
    type: entry.type,
    wire,
    models: models.map((model) => ({
      id: model.id,
      name: model.name,
      max_output_size: model.maxOutputSize,
      reasoning_key: model.reasoningKey,
      capability: {
        image_in: model.capability.image_in,
        video_in: model.capability.video_in,
        audio_in: model.capability.audio_in,
        thinking: model.capability.thinking,
        tool_use: model.capability.tool_use,
        max_context_tokens: model.capability.max_context_tokens,
        dynamically_loaded_tools: model.capability.dynamically_loaded_tools,
      },
    })),
  };
}
