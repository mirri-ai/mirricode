export {
  DeviceCodeExpiredError,
  DeviceCodeTimeoutError,
  OAuthConnectionError,
  OAuthError,
  OAuthUnauthorizedError,
  RetryableRefreshError,
} from './errors';

export type {
  DeviceAuthorization,
  DeviceHeaders,
  OAuthFlowConfig,
  OAuthStorageBackend,
  TokenInfo,
  TokenInfoWire,
} from './types';
export { tokenFromWire, tokenToWire } from './types';

export type { TokenStorage } from './storage';
export { FileTokenStorage } from './storage';

export type { DevicePollResult, RefreshOptions } from './oauth';
export { pollDeviceToken, refreshAccessToken, requestDeviceAuthorization } from './oauth';

export type { LoginOptions, OAuthManagerOptions, OAuthRefreshOutcome } from './oauth-manager';
export { OAuthManager, defaultRefreshThreshold, newInstanceId } from './oauth-manager';

export {
  assertMirriHostIdentity,
  createMirriDefaultHeaders,
  createMirriDeviceHeaders,
  createMirriDeviceId,
  createMirriUserAgent,
  MIRRICODE_CUSTOM_HEADERS_ENV,
  MIRRICODE_PLATFORM,
  parseMirriCodeCustomHeaders,
  readMirriDeviceId,
} from './identity';
export type { MirriHostIdentity, MirriIdentityOptions } from './identity';

export { MIRRICODE_FLOW_CONFIG } from './constants';

export {
  applyMirriManagedCodeLogoutConfig,
  applyManagedMirriCodeConfig,
  clearManagedMirriCodeConfig,
  fetchManagedMirriCodeModels,
  mirriCodeEnvBaseUrl,
  mirriCodeEnvOAuthHost,
  MIRRICODE_OAUTH_KEY,
  MIRRICODE_PLATFORM_ID,
  MIRRICODE_PROVIDER_NAME,
  ManagedMirriCodeModelsAuthError,
  provisionManagedMirriCodeConfig,
  resolveMirriCodeLoginAuth,
  resolveMirriCodeOAuthKey,
  resolveMirriCodeOAuthRef,
  resolveMirriCodeRuntimeAuth,
} from './managed-mirri-code';
export type {
  FetchManagedMirriCodeModelsOptions,
  MirriManagedCodeApplyResult,
  MirriManagedCodeCleanupResult,
  MirriManagedCodeProtocol,
  MirriManagedEnv,
  MirriManagedLoginAuth,
  ManagedMirriCodeModelInfo,
  ManagedMirriCodeProvisionResult,
  ManagedMirriConfigAdapter,
  ManagedMirriConfigShape,
  ManagedMirriOAuthRef,
  ManagedMirriOAuthRefInput,
  MirriManagedRuntimeAuth,
  ProvisionManagedMirriCodeConfigOptions,
} from './managed-mirri-code';

export {
  fetchManagedUsage,
  formatDuration,
  formatResetTime,
  isMirriManagedCode,
  mirriCodeBaseUrl,
  mirriCodeUsageUrl,
  parseManagedUsagePayload,
} from './managed-usage';
export type {
  FetchManagedUsageError,
  FetchManagedUsageResult,
  ParsedManagedUsage,
  UsageRow,
} from './managed-usage';

export { fetchSubmitFeedback, mirriCodeFeedbackUrl } from './managed-feedback';
export type {
  FetchSubmitFeedbackError,
  FetchSubmitFeedbackOk,
  FetchSubmitFeedbackResult,
  SubmitFeedbackBody,
} from './managed-feedback';

export {
  fetchCompleteFeedbackUpload,
  fetchCreateFeedbackUploadUrl,
  mirriCodeFeedbackUploadCompleteUrl,
  mirriCodeFeedbackUploadUrl,
} from './managed-feedback-upload';
export type {
  CompleteFeedbackUploadBody,
  CreateFeedbackUploadUrlBody,
  CreateFeedbackUploadUrlResponse,
  FetchCompleteFeedbackUploadResult,
  FetchCreateFeedbackUploadUrlResult,
  FetchFeedbackUploadError,
} from './managed-feedback-upload';

export {
  applyOpenPlatformConfig,
  capabilitiesForModel,
  fetchOpenPlatformModels,
  filterModelsByPrefix,
  getOpenPlatformById,
  isOpenPlatformId,
  OPEN_PLATFORMS,
  OpenPlatformApiError,
  removeOpenPlatformConfig,
} from './open-platform';
export type {
  ApplyOpenPlatformResult,
  OpenPlatformDefinition,
} from './open-platform';

export {
  applyCustomRegistryEntries,
  applyCustomRegistryProvider,
  capabilitiesFromCustomEntry,
  CustomRegistryApiError,
  CUSTOM_REGISTRY_DEFAULT_CAPABILITIES,
  CUSTOM_REGISTRY_DEFAULT_MAX_CONTEXT,
  fetchCustomRegistry,
  removeCustomRegistryProvider,
} from './custom-registry';
export type {
  CustomRegistryModelEntry,
  CustomRegistryProviderEntry,
  CustomRegistryProviderType,
  CustomRegistrySource,
} from './custom-registry';

export { MirriOAuthToolkit, resolveMirriTokenStorageName } from './toolkit';
export type {
  AuthManagedUsageResult,
  AuthProviderStatus,
  AuthStatus,
  BearerTokenProvider,
  MirriOAuthLoginOptions,
  MirriOAuthLoginResult,
  MirriOAuthLogoutResult,
  MirriOAuthTokenRef,
  MirriOAuthToolkitOptions,
} from './toolkit';

export { refreshProviderModels } from './refreshProviderModels';
export type {
  ProviderChange,
  RefreshProviderHost,
  RefreshProviderOptions,
  RefreshProviderScope,
  RefreshResult,
} from './refreshProviderModels';
