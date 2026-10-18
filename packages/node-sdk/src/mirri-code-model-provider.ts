import {
  ErrorCodes,
  MirriError,
  resolveMirriHome,
  type Logger,
  type ModelProvider,
  type ResolvedRuntimeProvider,
} from '@mirri-ai/agent-core';
import {
  APIStatusError,
  createMirriDefaultHeaders,
  parseMirriCodeCustomHeaders,
  UNKNOWN_CAPABILITY,
  type MirriHostIdentity,
  type ProviderConfig as KosongProviderConfig,
  type ProviderRequestAuth,
} from '@mirri-ai/kosong';

/** Default base URL for the Mirri coding API (env `MIRRICODE_BASE_URL` wins). */
const DEFAULT_MIRRICODE_BASE_URL = 'https://api.kimi.com/coding/v1';

export interface MirriForCodingProviderOptions extends MirriHostIdentity {
  /** API key for the Mirri coding API, served as the bearer credential at request time. */
  readonly apiKey?: string;
  readonly homeDir?: string;
  readonly model?: string;
  readonly baseUrl?: string;
  readonly promptCacheKey?: string;
  readonly defaultHeaders?: Record<string, string>;
}

export class MirriForCodingProvider implements ModelProvider {
  private readonly model: string;
  private readonly baseUrl: string;
  private readonly promptCacheKey: string | undefined;
  private readonly defaultHeaders: Record<string, string> | undefined;
  private readonly homeDir: string;
  private readonly identity: MirriHostIdentity;
  private readonly apiKey: string | undefined;

  constructor(options: MirriForCodingProviderOptions) {
    this.model = options.model ?? 'kimi-for-coding';
    this.baseUrl = options.baseUrl ?? mirriCodeBaseUrl();
    this.promptCacheKey = options.promptCacheKey;
    this.defaultHeaders = options.defaultHeaders;
    this.homeDir = resolveMirriHome(options.homeDir);
    this.identity = {
      productName: options.productName,
      version: options.version,
      platform: options.platform,
      userAgentSuffix: options.userAgentSuffix,
    };
    this.apiKey = options.apiKey;
  }

  get defaultModel(): string {
    return this.model;
  }

  resolveProviderConfig(model: string): ResolvedRuntimeProvider {
    if (model !== this.model) {
      throw new MirriError(
        ErrorCodes.CONFIG_INVALID,
        `Model "${model}" is not supported by MirriForCodingProvider.`,
      );
    }

    const provider: KosongProviderConfig = {
      type: 'openai',
      model: this.model,
      baseUrl: this.baseUrl,
      defaultHeaders: {
        ...parseMirriCodeCustomHeaders(),
        ...createMirriDefaultHeaders({
          homeDir: this.homeDir,
          ...this.identity,
        }),
        ...this.defaultHeaders,
      },
    };

    return {
      providerName: 'kimi-for-coding',
      provider,
      modelCapabilities: UNKNOWN_CAPABILITY,
      type: 'openai',
      protocol: undefined,
    };
  }

  resolveAuth(_model: string, _options?: { readonly log?: Logger }) {
    return async <T>(request: (auth: ProviderRequestAuth) => Promise<T>): Promise<T> => {
      const apiKey = this.apiKey;
      if (apiKey === undefined || apiKey.length === 0) {
        throw new MirriError(
          ErrorCodes.AUTH_LOGIN_REQUIRED,
          'MirriForCodingProvider requires an API key; pass the "apiKey" option to the provider.',
        );
      }
      try {
        return await request({ apiKey });
      } catch (error) {
        if (error instanceof APIStatusError && error.statusCode === 401) {
          throw new MirriError(
            ErrorCodes.AUTH_LOGIN_REQUIRED,
            'The Mirri coding API rejected the API key. Check the configured key.',
            { cause: error },
          );
        }
        throw error;
      }
    };
  }
}

function mirriCodeBaseUrl(): string {
  return (process.env['MIRRICODE_BASE_URL'] ?? DEFAULT_MIRRICODE_BASE_URL).replace(/\/+$/, '');
}