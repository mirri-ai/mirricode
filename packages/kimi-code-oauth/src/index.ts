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
  applyManagedApiKeyProviderModels,
  applyManagedMirriCodeLogoutConfig,
  applyManagedMirriCodeConfig,
  clearManagedMirriCodeConfig,
  fetchManagedMirriCodeModels,
  kimiCodeEnvBaseUrl,
  kimiCodeEnvOAuthHost,
  MIRRICODE_OAUTH_KEY,
  MIRRICODE_PLATFORM_ID,
  MIRRICODE_PROVIDER_NAME,
  ManagedMirriCodeModelsAuthError,
  provisionManagedMirriCodeConfig,
  resolveMirriCodeLoginAuth,
  resolveMirriCodeOAuthKey,
  resolveMirriCodeOAuthRef,
  resolveMirriCodeRuntimeAuth,
  toManagedModelAlias,
} from './managed-mirri-code';
export type {
  FetchManagedMirriCodeModelsOptions,
  ManagedMirriCodeApplyResult,
  ManagedMirriCodeCleanupResult,
  ManagedMirriCodeProtocol,
  ManagedMirriEnv,
  ManagedMirriLoginAuth,
  ManagedMirriCodeModelInfo,
  ManagedMirriCodeProvisionResult,
  ManagedMirriConfigAdapter,
  ManagedMirriConfigShape,
  ManagedMirriOAuthRef,
  ManagedMirriOAuthRefInput,
  ManagedMirriRuntimeAuth,
  ProvisionManagedMirriCodeConfigOptions,
} from './managed-mirri-code';

export {
  fetchManagedUserInfo,
  kimiCodeUserInfoUrl,
  managedUserInfoPhoneSchema,
  managedUserInfoResultSchema,
  managedUserInfoSchema,
  parseManagedUserInfoPayload,
} from './managed-userinfo';
export type {
  FetchManagedUserInfoError,
  FetchManagedUserInfoResult,
  ManagedUserInfo,
  ManagedUserInfoPhone,
  ManagedUserInfoResult,
} from './managed-userinfo';

export {
  fetchManagedUsage,
  formatDuration,
  isManagedMirriCode,
  isManagedMirriCodeBaseUrl,
  kimiCodeBaseUrl,
  kimiCodeUsageUrl,
  parseManagedUsagePayload,
} from './managed-usage';
export type {
  FetchManagedUsageError,
  FetchManagedUsageResult,
  ParsedManagedUsage,
  UsageRow,
  UsageWindow,
} from './managed-usage';

export { fetchSubmitFeedback, kimiCodeFeedbackUrl } from './managed-feedback';
export type {
  FetchSubmitFeedbackError,
  FetchSubmitFeedbackOk,
  FetchSubmitFeedbackResult,
  SubmitFeedbackBody,
} from './managed-feedback';

export {
  fetchCompleteFeedbackUpload,
  fetchCreateFeedbackUploadUrl,
  kimiCodeFeedbackUploadCompleteUrl,
  kimiCodeFeedbackUploadUrl,
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
  FetchCustomRegistryOptions,
} from './custom-registry';

export { MirriOAuthToolkit, resolveMirriTokenStorageName } from './toolkit';
export type {
  AuthManagedUserInfoResult,
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
