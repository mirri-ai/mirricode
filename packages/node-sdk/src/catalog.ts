import type { MirriConfig, ModelAlias } from '@mirri-ai/agent-core';
import {
  CatalogFetchError,
  DEFAULT_CATALOG_URL,
  catalogBaseUrl,
  catalogProviderModels,
  fetchCatalog,
  inferWireType,
  loadBuiltInCatalog,
  type Catalog,
  type CatalogModel,
  type CatalogProviderEntry,
  type ModelCapability,
  type ProviderType,
} from '@mirri-ai/kosong';

export {
  CatalogFetchError,
  DEFAULT_CATALOG_URL,
  catalogBaseUrl,
  catalogProviderModels,
  fetchCatalog,
  inferWireType,
  loadBuiltInCatalog,
};
export type { Catalog, CatalogModel, CatalogProviderEntry };

function capabilityToStrings(capability: ModelCapability): string[] | undefined {
  const caps: string[] = [];
  if (capability.image_in) caps.push('image_in');
  if (capability.video_in) caps.push('video_in');
  if (capability.audio_in) caps.push('audio_in');
  if (capability.thinking) caps.push('thinking');
  if (capability.tool_use) caps.push('tool_use');
  if (capability.dynamically_loaded_tools === true) caps.push('dynamically_loaded_tools');
  return caps.length > 0 ? caps : undefined;
}

/** Builds a mirri-code model alias from a normalized catalog model. */
export function catalogModelToAlias(providerId: string, model: CatalogModel): ModelAlias {
  return {
    provider: providerId,
    model: model.id,
    maxContextSize: model.capability.max_context_tokens,
    maxOutputSize: model.maxOutputSize,
    capabilities: capabilityToStrings(model.capability),
    displayName: model.name,
    reasoningKey: model.reasoningKey,
  };
}

export interface ApplyCatalogProviderOptions {
  readonly providerId: string;
  readonly wire: ProviderType;
  readonly baseUrl?: string;
  readonly apiKey: string;
  readonly models: readonly CatalogModel[];
  readonly selectedModelId: string;
  readonly thinking: boolean;
}

/**
 * Writes a catalog-selected provider and its model aliases into `config` and
 * marks it the default. Model metadata (context, output limit, capabilities)
 * comes from the catalog, so the user does not hand-write it. Returns the
 * default model key.
 *
 * NOTE: the same-provider cleanup below mutates the passed-in `config` only.
 * It clears stale aliases on disk solely when the caller overwrites the whole
 * config. Callers persisting via `setConfig` — a deep-merge patch that cannot
 * delete keys — must call `removeProvider` first, or removed aliases reappear
 * after the merge.
 */
export function applyCatalogProvider(
  config: MirriConfig,
  options: ApplyCatalogProviderOptions,
): { defaultModel: string } {
  config.providers[options.providerId] = {
    type: options.wire,
    baseUrl: options.baseUrl,
    apiKey: options.apiKey,
  };

  const models = config.models ?? {};
  for (const [key, alias] of Object.entries(models)) {
    if (alias.provider === options.providerId) delete models[key];
  }
  for (const model of options.models) {
    models[`${options.providerId}/${model.id}`] = catalogModelToAlias(options.providerId, model);
  }
  config.models = models;

  const defaultModel = `${options.providerId}/${options.selectedModelId}`;
  config.defaultModel = defaultModel;
  config.thinking = { ...config.thinking, enabled: options.thinking };
  return { defaultModel };
}
