import type { Logger } from '#/logging/types';
import type { ProviderConfig as KosongProviderConfig, ModelCapability, ProviderRequestAuth } from '@mirri-ai/kosong';
import { APIStatusError, getModelCapability, UNKNOWN_CAPABILITY } from '@mirri-ai/kosong';
import { parseMirriCodeCustomHeaders } from '@mirri-ai/mirri-code-oauth';
import {
  effectiveModelAlias,
  type MirriConfig,
  type ModelAlias,
  type OAuthRef,
  type ProviderConfig,
  type ProviderType,
} from '../config';
import { expandEnvString } from '../config/env-expand';
import { ErrorCodes, isMirriError, MirriError } from '../errors';

export interface BearerTokenProvider {
  getAccessToken(options?: { readonly force?: boolean }): Promise<string>;
}

export type OAuthTokenProviderResolver = (
  providerName: string,
  oauthRef?: OAuthRef,
) => BearerTokenProvider | undefined;

export interface ResolvedRuntimeProvider {
  readonly providerName: string;
  readonly provider: KosongProviderConfig;
  readonly modelCapabilities: ModelCapability;
  /** Declared 'always_thinking' capability — the model cannot disable thinking. */
  readonly alwaysThinking?: boolean;
  /** Efforts the model advertises (e.g. ["low", "high", "max"]). */
  readonly supportEfforts?: readonly string[];
  /** The catalog default effort for a model, if declared. */
  readonly defaultEffort?: string;
  readonly maxOutputSize?: number;
  /** Configured provider wire type (`provider.type`), before any model-level protocol override. */
  readonly type: ProviderType;
  /** Model-level protocol override (`alias.protocol`); when set, takes precedence over `type` for transport selection. */
  readonly protocol: ModelAlias['protocol'];
}

interface ProviderManagerOptions {
  readonly config: MirriConfig | (() => MirriConfig);
  readonly mirriRequestHeaders?: Record<string, string>;
  readonly resolveOAuthTokenProvider?: OAuthTokenProviderResolver;
  readonly promptCacheKey?: string;
}

type AuthorizedRequest = <T>(
  request: (auth: ProviderRequestAuth) => Promise<T>,
) => Promise<T>;

export interface ModelProvider {
  readonly defaultModel?: string;
  resolveProviderConfig(model: string): ResolvedRuntimeProvider;
  resolveAuth?(model: string, options?: { readonly log?: Logger }): AuthorizedRequest | undefined;
}

export class SingleModelProvider implements ModelProvider {
  constructor(
    private readonly providerConfig: KosongProviderConfig,
    private readonly modelCapabilities: ModelCapability = UNKNOWN_CAPABILITY,
  ) {}

  get defaultModel(): string {
    return this.providerConfig.model;
  }

  resolveProviderConfig(model: string): ResolvedRuntimeProvider {
    if (model !== this.providerConfig.model) {
      throw new MirriError(
        ErrorCodes.CONFIG_INVALID,
        `Model "${model}" is not supported by SingleModelProvider.`,
      );
    }
    return {
      modelCapabilities: this.modelCapabilities,
      providerName: 'single-model-provider',
      provider: this.providerConfig,
      type: this.providerConfig.type,
      protocol: undefined,
    };
  }
}

export class ProviderManager implements ModelProvider {
  constructor(private readonly options: ProviderManagerOptions) {}

  private get config(): MirriConfig {
    const { config } = this.options;
    return typeof config === 'function' ? config() : config;
  }

  resolveProviderConfig(model: string): ResolvedRuntimeProvider {
    const alias = this.config.models?.[model];
    if (alias === undefined) {
      throw new MirriError(
        ErrorCodes.CONFIG_INVALID,
        `Model "${model}" is not configured in config.toml. Add a [models."${model}"] entry with max_context_size.`,
      );
    }

    const providerName = alias.provider ?? this.config.defaultProvider;
    if (providerName === undefined) {
      throw new MirriError(
        ErrorCodes.CONFIG_INVALID,
        `Model "${model}" must define a provider in config.toml.`,
      );
    }

    const providerConfig = this.config.providers[providerName];
    if (providerConfig === undefined) {
      throw new MirriError(
        ErrorCodes.CONFIG_INVALID,
        `Provider "${providerName}" for model "${model}" is not configured.`,
      );
    }

    const effectiveAlias = effectiveModelAlias(alias, providerConfig.type);

    if (!Number.isInteger(effectiveAlias.maxContextSize) || effectiveAlias.maxContextSize <= 0) {
      throw new MirriError(
        ErrorCodes.CONFIG_INVALID,
        `Model "${model}" must define a positive max_context_size in config.toml.`,
      );
    }

    const provider = toKosongProviderConfig(
      providerConfig,
      alias.model,
      alias.protocol,
      this.options.mirriRequestHeaders,
      effectiveAlias.maxOutputSize,
      effectiveAlias.reasoningKey,
      this.options.promptCacheKey,
      effectiveAlias.supportEfforts,
      effectiveAlias.adaptiveThinking,
      alias.betaApi,
    );

    return {
      providerName,
      provider,
      modelCapabilities: resolveModelCapabilities(effectiveAlias, provider),
      alwaysThinking: (effectiveAlias.capabilities ?? []).some(
        (c) => c.trim().toLowerCase() === 'always_thinking',
      ),
      supportEfforts: effectiveAlias.supportEfforts,
      defaultEffort: effectiveAlias.defaultEffort,
      maxOutputSize: effectiveAlias.maxOutputSize,
      type: providerConfig.type,
      protocol: alias.protocol,
    };
  }

  resolveAuth(
    model: string,
    options?: { readonly log?: Logger },
  ): AuthorizedRequest | undefined {
    const { providerName } = this.resolveProviderConfig(model);
    const providerConfig = this.config.providers[providerName];
    if (providerConfig?.oauth === undefined) return undefined;

    if (providerApiKey(providerConfig) !== undefined) {
      // oauth + apiKey on the same provider makes request auth ambiguous:
      // provider construction would prefer apiKey while runtime auth resolves
      // OAuth. Reject it so misconfiguration surfaces at model resolution.
      throw new MirriError(
        ErrorCodes.CONFIG_INVALID,
        `Provider "${providerName}" has both apiKey and oauth set in config.toml — they are mutually exclusive. Remove one.`,
      );
    }

    const loginRequired = (cause?: unknown): MirriError =>
      new MirriError(
        ErrorCodes.AUTH_LOGIN_REQUIRED,
        `OAuth provider "${providerName}" requires login before it can be used.`,
        cause === undefined ? undefined : { cause },
      );

    const tokenProvider = this.options.resolveOAuthTokenProvider?.(providerName, providerConfig.oauth);
    if (tokenProvider === undefined) {
      return async () => {
        throw loginRequired();
      };
    }

    const log = options?.log;
    const fetchAuth = async (force: boolean): Promise<ProviderRequestAuth> => {
      let apiKey: string;
      try {
        apiKey = await tokenProvider.getAccessToken(force ? { force: true } : undefined);
      } catch (error) {
        // login-required is an expected state (the user must /login); don't
        // warn. Other failures (connection errors, etc.) are logged once for
        // diagnosis and then propagated — chatWithRetry does not retry them.
        if (!isMirriError(error) || error.code !== ErrorCodes.AUTH_LOGIN_REQUIRED) {
          log?.warn('oauth token fetch failed', { providerName, error });
        }
        throw error;
      }
      if (apiKey.trim().length === 0) throw loginRequired();
      return { apiKey };
    };

    return async (request) => {
      let auth = await fetchAuth(false);
      for (let refreshed = false; ; refreshed = true) {
        try {
          return await request(auth);
        } catch (error) {
          if (!(error instanceof APIStatusError) || error.statusCode !== 401) throw error;
          if (refreshed) {
            const reason = error.message.replaceAll('\r', '');
            throw new MirriError(
              ErrorCodes.PROVIDER_AUTH_ERROR,
              reason.length > 0 ? reason : 'OAuth provider credentials were rejected.',
              {
                cause: error,
                details: { statusCode: error.statusCode, requestId: error.requestId },
              },
            );
          }
          auth = await fetchAuth(true);
        }
      }
    };
  }
}

function resolveModelCapabilities(
  alias: ModelAlias,
  provider: KosongProviderConfig,
): ModelCapability {
  const declared = new Set((alias.capabilities ?? []).map((c) => c.trim().toLowerCase()));
  const detected = getModelCapability(provider.type, provider.model);

  return {
    image_in: declared.has('image_in') || detected.image_in,
    video_in: declared.has('video_in') || detected.video_in,
    audio_in: declared.has('audio_in') || detected.audio_in,
    thinking: declared.has('thinking') || declared.has('always_thinking') || detected.thinking,
    tool_use: declared.has('tool_use') || detected.tool_use,
    max_context_tokens: alias.maxContextSize,
    // Message-level tool declarations (dynamically_loaded_tools progressive disclosure).
    // Every field here must be merged explicitly — a capability registered in
    // kosong that is not forwarded here never reaches the agent.
    dynamically_loaded_tools: declared.has('dynamically_loaded_tools') || detected.dynamically_loaded_tools === true,
  };
}

function toKosongProviderConfig(
  provider: ProviderConfig,
  model: string,
  modelProtocol: ModelAlias['protocol'],
  mirriRequestHeaders: Record<string, string> | undefined,
  maxOutputSize: number | undefined,
  reasoningKey: string | undefined,
  promptCacheKey: string | undefined,
  supportEfforts: readonly string[] | undefined,
  adaptiveThinking: boolean | undefined,
  betaApi: boolean | undefined,
): KosongProviderConfig {
  const effectiveType = modelProtocol === 'anthropic' ? 'anthropic' : provider.type;
  const envCustomHeaders = parseMirriCodeCustomHeaders();
  // Mirri providers (no custom baseUrl) get the full identity header set;
  // third-party providers only receive User-Agent.
  const identityHeaders = isMirriProvider(provider)
    ? (mirriRequestHeaders ?? {})
    : mirriUserAgentHeader(mirriRequestHeaders);
  switch (effectiveType) {
    case 'anthropic': {
      const baseUrl = providerValue(provider.baseUrl, provider.env, 'ANTHROPIC_BASE_URL');
      return {
        type: 'anthropic',
        model,
        baseUrl:
          modelProtocol === 'anthropic' && baseUrl !== undefined
            ? baseUrl.replace(/\/v1\/?$/, '')
            : baseUrl,
        apiKey: providerApiKey(provider),
        ...(maxOutputSize !== undefined ? { defaultMaxTokens: maxOutputSize } : {}),
        ...(supportEfforts !== undefined ? { supportEfforts } : {}),
        ...(adaptiveThinking !== undefined ? { adaptiveThinking } : {}),
        ...((provider.type as string) === 'kimi' ? { kimiThinking: true } : {}),
        ...(betaApi !== undefined ? { betaApi } : {}),
        // Session affinity: Anthropic's analog of OpenAI `prompt_cache_key` is
        // `metadata.user_id` on the Messages API (cache-affinity / end-user id).
        ...(promptCacheKey !== undefined ? { metadata: { user_id: promptCacheKey } } : {}),
        // When a Mirri provider is routed through the Anthropic transport
        ...defaultHeadersField({
          ...envCustomHeaders,
          ...identityHeaders,
          ...provider.customHeaders,
        }),
      };
    }
    case 'openai':
      return {
        type: 'openai',
        model,
        baseUrl: providerValue(provider.baseUrl, provider.env, 'OPENAI_BASE_URL'),
        apiKey: providerApiKey(provider),
        reasoningKey,
        ...(supportEfforts !== undefined ? { supportEfforts } : {}),
        ...(promptCacheKey !== undefined
          ? { generationKwargs: { prompt_cache_key: promptCacheKey } }
          : {}),
        ...defaultHeadersField({
          ...envCustomHeaders,
          ...identityHeaders,
          ...provider.customHeaders,
        }),
      };
    case 'google-genai':
      return {
        type: 'google-genai',
        model,
        baseUrl: providerValue(provider.baseUrl, provider.env, 'GOOGLE_GEMINI_BASE_URL'),
        apiKey: providerApiKey(provider),
        ...defaultHeadersField({
          ...envCustomHeaders,
          ...identityHeaders,
          ...provider.customHeaders,
        }),
      };
    case 'openai_responses':
      return {
        type: 'openai_responses',
        model,
        baseUrl: providerValue(provider.baseUrl, provider.env, 'OPENAI_BASE_URL'),
        apiKey: providerApiKey(provider),
        ...defaultHeadersField({
          ...envCustomHeaders,
          ...identityHeaders,
          ...provider.customHeaders,
        }),
      };
    case 'vertexai': {
      // Resolve the effective endpoint once (config `base_url` or the
      // GOOGLE_VERTEX_BASE_URL env fallback) and use it for BOTH forwarding and
      // location detection, so the env fallback behaves exactly like
      // `base_url` — including deriving the region from an
      // `*-aiplatform.googleapis.com` host for the service-account path.
      const baseUrl = providerValue(provider.baseUrl, provider.env, 'GOOGLE_VERTEX_BASE_URL');
      const useServiceAccount = hasVertexAIServiceEnv(provider, baseUrl);
      return {
        type: 'vertexai',
        model,
        vertexai: useServiceAccount,
        baseUrl,
        apiKey: useServiceAccount ? undefined : providerApiKey(provider),
        project: vertexAIProject(provider),
        location: vertexAILocation(provider, baseUrl),
        ...defaultHeadersField({
          ...envCustomHeaders,
          ...identityHeaders,
          ...provider.customHeaders,
        }),
      };
    }
    default: {
      const exhaustive: never = effectiveType;
      throw new MirriError(
        ErrorCodes.MODEL_CONFIG_INVALID,
        `Unsupported provider type: ${String(exhaustive)}`,
      );
    }
  }
}

// Returns a fresh `defaultHeaders` field for a kosong provider config so
// resolved instances never share a header object. Omits the key entirely when
// there are no headers — callers and tests rely on `'defaultHeaders' in provider`.
function defaultHeadersField(
  headers: Record<string, string> | undefined,
): { defaultHeaders?: Record<string, string> } {
  if (headers === undefined || Object.keys(headers).length === 0) return {};
  return { defaultHeaders: { ...headers } };
}

// Extract just the `User-Agent` from the Mirri identity headers so non-Mirri
// providers (OpenAI, Anthropic, Google, Vertex) also identify as
// `mirri-code-cli/<version>` without leaking the `X-Msh-*` device identity
// headers to third-party endpoints. The full `mirriRequestHeaders` set stays
// reserved for the Mirri transport (and the Mirri-routed Anthropic transport),
// where upstream is the managed Mirri endpoint.
function mirriUserAgentHeader(
  mirriRequestHeaders: Record<string, string> | undefined,
): Record<string, string> {
  const userAgent = mirriRequestHeaders?.['User-Agent'];
  return userAgent === undefined ? {} : { 'User-Agent': userAgent };
}

// Mirri providers are identified by having no explicit baseUrl — they talk to
// the managed Mirri endpoint which expects the full device identity headers.
function isMirriProvider(provider: ProviderConfig): boolean {
  return provider.baseUrl === undefined;
}

function providerApiKey(provider: ProviderConfig): string | undefined {
  const expandedApiKey =
    provider.apiKey !== undefined ? expandEnvString(provider.apiKey) : undefined;
  switch (provider.type) {
    case 'anthropic':
      return providerValue(expandedApiKey, provider.env, 'ANTHROPIC_API_KEY');
    case 'openai':
    case 'openai_responses':
      return providerValue(expandedApiKey, provider.env, 'OPENAI_API_KEY');
    case 'google-genai':
      return providerValue(expandedApiKey, provider.env, 'GOOGLE_API_KEY');
    case 'vertexai':
      return (
        nonEmptyString(expandedApiKey) ??
        envValue(provider.env, 'VERTEXAI_API_KEY') ??
        envValue(provider.env, 'GOOGLE_API_KEY')
      );
    default: {
      const exhaustive: never = provider.type;
      throw new MirriError(
        ErrorCodes.MODEL_CONFIG_INVALID,
        `Unsupported provider type: ${String(exhaustive)}`,
      );
    }
  }
}

function hasVertexAIServiceEnv(provider: ProviderConfig, baseUrl: string | undefined): boolean {
  return vertexAIProject(provider) !== undefined && vertexAILocation(provider, baseUrl) !== undefined;
}

function vertexAIProject(provider: ProviderConfig): string | undefined {
  return envValue(provider.env, 'GOOGLE_CLOUD_PROJECT');
}

function vertexAILocation(
  provider: ProviderConfig,
  baseUrl: string | undefined,
): string | undefined {
  return envValue(provider.env, 'GOOGLE_CLOUD_LOCATION') ?? locationFromVertexAIBaseUrl(baseUrl);
}

function providerValue(
  configured: string | undefined,
  env: Record<string, string> | undefined,
  envKey: string,
): string | undefined {
  return nonEmptyString(configured) ?? envValue(env, envKey);
}

function envValue(env: Record<string, string> | undefined, key: string): string | undefined {
  return nonEmptyString(env?.[key]);
}

function nonEmptyString(value: string | undefined): string | undefined {
  const trimmed = value?.trim();
  return trimmed === undefined || trimmed.length === 0 ? undefined : trimmed;
}

function locationFromVertexAIBaseUrl(baseUrl: string | undefined): string | undefined {
  const url = nonEmptyString(baseUrl);
  if (url === undefined) return undefined;
  try {
    const host = new URL(url).hostname;
    const suffix = '-aiplatform.googleapis.com';
    return host.endsWith(suffix) ? nonEmptyString(host.slice(0, -suffix.length)) : undefined;
  } catch {
    return undefined;
  }
}
