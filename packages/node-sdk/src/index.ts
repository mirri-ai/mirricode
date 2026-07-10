export { MirriHarness } from '#/mirri-harness';
export type { MirriHarnessRuntimeOptions } from '#/mirri-harness';
export { Session } from '#/session';
export { MirriAuthFacade } from '#/auth';
export { createMirriHarness, SDKRpcClient, type SDKRpcClientOptions } from '#/sdk-rpc-client';
export {
  createMirriConfigRpc,
  MirriConfigRpcClient,
  type MirriConfigRpc,
  type MirriConfigValidationIssue,
  type MirriConfigValidationPathSegment,
  type ResolveMirriConfigPathInput,
  type ValidateMirriConfigTomlInput,
} from '#/config-rpc';
export { SDKRpcClientBase } from '#/rpc';
export { MirriForCodingProvider } from '#/mirri-code-model-provider';
export type { MirriForCodingProviderOptions } from '#/mirri-code-model-provider';

export {
  applyCatalogProvider,
  catalogBaseUrl,
  catalogModelToAlias,
  catalogProviderModels,
  CatalogFetchError,
  DEFAULT_CATALOG_URL,
  fetchCatalog,
  inferWireType,
  loadBuiltInCatalog,
} from '#/catalog';
export type {
  ApplyCatalogProviderOptions,
  Catalog,
  CatalogModel,
  CatalogProviderEntry,
} from '#/catalog';

export {
  ErrorCodes,
  MirriError,
  type MirriErrorCode,
  type MirriErrorInfo,
  type MirriErrorOptions,
  type MirriErrorPayload,
  MIRRI_ERROR_INFO,
  fromMirriErrorPayload,
  isMirriError,
  toMirriErrorPayload,
} from '@mirri-ai/agent-core';

// Diagnostic logging — public surface only.
// RootLogger / getRootLogger / LoggingConfig stay inside agent-core.
export {
  flushDiagnosticLogs,
  log,
  redact,
  resolveGlobalLogPath,
  resolveMirriHome,
} from '@mirri-ai/agent-core';
export type { LogContext, LogLevel, LogPayload, Logger } from '@mirri-ai/agent-core';

// Host-side config helpers — safe config reader + config path resolution, used
// by hosts (e.g. the CLI's server telemetry bootstrap) that need to inspect
// config without spinning up a full MirriCore.
export { effectiveModelAlias, loadRuntimeConfigSafe, resolveConfigPath } from '@mirri-ai/agent-core';

// Process-wide HTTP proxy bootstrap — installed once at CLI startup so all
// outbound fetch honors HTTP_PROXY / HTTPS_PROXY / NO_PROXY.
export { installGlobalProxyDispatcher } from '@mirri-ai/agent-core';

// Image compression — ingestion sites (e.g. the CLI's clipboard paste, the ACP
// adapter) shrink oversized images while constructing the content part, before
// it enters a prompt. Best effort: returns the original on any failure.
// Compression is never silent: buildImageCompressionCaption renders the note
// placed next to a compressed image, and persistOriginalImage keeps the
// pre-compression bytes readable (ReadMediaFile + region) for detail.
export {
  buildImageCompressionCaption,
  compressImageForModel,
  compressBase64ForModel,
  persistOriginalImage,
  sessionMediaOriginalsDir,
  IMAGE_BYTE_BUDGET,
  MAX_IMAGE_EDGE_PX,
} from '@mirri-ai/agent-core';
export { ImageLimits } from '@mirri-ai/agent-core';
export type {
  CompressImageOptions,
  CompressImageResult,
  CompressBase64Result,
  ImageCompressionCaptionInput,
  ImageCompressionTelemetry,
} from '@mirri-ai/agent-core';

// Experimental feature flags — types only. Resolved values come from
// `MirriHarness.getExperimentalFeatures()` over RPC, not from a re-exported runtime value.
export type {
  ExperimentalFeatureState,
  ExperimentalFlagMap,
  ExperimentalFlagSource,
  FlagDefinition,
  FlagDefinitionInput,
  FlagId,
  FlagSurface,
} from '@mirri-ai/agent-core';

export type {
  MirriAuthCompleteFeedbackUploadInput,
  MirriAuthCompleteFeedbackUploadPart,
  MirriAuthCreateFeedbackUploadUrlInput,
  MirriAuthCreateFeedbackUploadUrlOk,
  MirriAuthCreateFeedbackUploadUrlResult,
  MirriAuthFeedbackUploadPart,
  MirriAuthLoginResult,
  MirriAuthLogoutResult,
  MirriAuthSubmitFeedbackInput,
} from '#/auth';

export * from '#/events';
export type * from '#/types';
