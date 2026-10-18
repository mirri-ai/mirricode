// Message types
export {
  createAssistantMessage,
  createToolMessage,
  createUserMessage,
  extractText,
  isContentPart,
  isToolCall,
  isToolCallPart,
  mergeInPlace,
} from './message';
export type {
  AudioURLPart,
  ContentPart,
  ImageURLPart,
  Message,
  Role,
  StreamedMessagePart,
  TextPart,
  ThinkPart,
  ToolCall,
  ToolCallPart,
  VideoURLPart,
} from './message';

// Provider interfaces
export * from './provider';
export { createProvider, getModelCapability } from './providers';
export type { ProviderConfig, ProviderType } from './providers';

// Model capability matrix
export { UNKNOWN_CAPABILITY, isUnknownCapability } from './capability';
export type { ModelCapability } from './capability';

// Model catalog (models.dev-style) metadata
export {
  CatalogFetchError,
  DEFAULT_CATALOG_URL,
  catalogBaseUrl,
  catalogModelToCapability,
  catalogProviderModels,
  fetchCatalog,
  inferWireType,
  loadBuiltInCatalog,
} from './catalog';
export type { Catalog, CatalogModel, CatalogModelEntry, CatalogProviderEntry } from './catalog';

// Core functions
export { generate } from './generate';
export type { GenerateCallbacks, GenerateResult } from './generate';

// Tool wire schema
export type { Tool } from './tool';

// Token usage
export { addUsage, emptyUsage, grandTotal, inputTotal } from './usage';
export type { TokenUsage } from './usage';

// Errors
export {
  APIConnectionError,
  APIContextOverflowError,
  APIEmptyResponseError,
  APIProviderRateLimitError,
  APIRequestTooLargeError,
  APIStatusError,
  APITimeoutError,
  ChatProviderError,
  isContextOverflowStatusError,
  isImageFormatError,
  isProviderRateLimitError,
  isRecoverableRequestStructureError,
  isRetryableGenerateError,
  isToolExchangeAdjacencyError,
  parseRetryAfterMs,
} from './errors';

// Host identity header factories (non-OAuth utilities)
export {
  MIRRICODE_CUSTOM_HEADERS_ENV,
  MIRRICODE_PLATFORM,
  assertMirriHostIdentity,
  createMirriDefaultHeaders,
  createMirriDeviceHeaders,
  createMirriDeviceId,
  createMirriUserAgent,
  parseMirriCodeCustomHeaders,
  readMirriDeviceId,
} from './host-identity';
export type { DeviceHeaders, MirriHostIdentity, MirriIdentityOptions } from './host-identity';

// API error message extraction
export { extractApiErrorMessage, readApiErrorMessage } from './api-error';

// Shared provider config shapes (non-OAuth)
export type {
  ModelInfo,
  ModelProtocol,
  ModelAlias,
  ModelAliasOverrides,
  ThinkingShape,
  ProviderConfigShape,
  SupportsThinkingType,
} from './provider-types';

// Model alias merge helpers
export {
  CUSTOM_REGISTRY_MODEL_FIELDS,
  OPEN_PLATFORM_MODEL_FIELDS,
  mergeRefreshedModelAlias,
} from './model-alias-merge';

// Open platforms (Kimi Platform API-key providers)
export {
  OPEN_PLATFORMS,
  OpenPlatformApiError,
  applyOpenPlatformConfig,
  capabilitiesForModel,
  fetchOpenPlatformModels,
  filterModelsByPrefix,
  getOpenPlatformById,
  isOpenPlatformId,
  removeOpenPlatformConfig,
} from './open-platform';
export type { ApplyOpenPlatformResult, OpenPlatformDefinition } from './open-platform';

// Custom registries (models.dev-style api.json providers)
export {
  CUSTOM_REGISTRY_DEFAULT_CAPABILITIES,
  CUSTOM_REGISTRY_DEFAULT_MAX_CONTEXT,
  CustomRegistryApiError,
  applyCustomRegistryEntries,
  applyCustomRegistryProvider,
  capabilitiesFromCustomEntry,
  fetchCustomRegistry,
  removeCustomRegistryProvider,
} from './custom-registry';
export type {
  CustomRegistryModelEntry,
  CustomRegistryProviderEntry,
  CustomRegistryProviderType,
  CustomRegistrySource,
  FetchCustomRegistryOptions,
} from './custom-registry';

// Provider model refresh orchestration
export { refreshProviderModels } from './refresh-providers';
export type {
  ProviderChange,
  RefreshProviderHost,
  RefreshProviderOptions,
  RefreshResult,
} from './refresh-providers';

/**
 * Concrete provider adapters stay off the root barrel because their SDK type
 * graphs pollute downstream declaration bundles. Import them from subpaths:
 * `@mirri-ai/kosong/providers/kimi`,
 * `@mirri-ai/kosong/providers/openai-legacy`, etc.
 */
