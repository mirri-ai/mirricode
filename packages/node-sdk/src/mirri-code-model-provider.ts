import {
  ErrorCodes,
  MirriError,
  resolveMirriHome,
  type Logger,
  type ModelProvider,
  type ResolvedRuntimeProvider,
} from '@mirri-ai/agent-core';
import {
  createMirriDefaultHeaders,
  MIRRICODE_FLOW_CONFIG,
  MIRRICODE_PROVIDER_NAME,
  MirriOAuthToolkit,
  mirriCodeBaseUrl,
  parseMirriCodeCustomHeaders,
  resolveMirriCodeOAuthRef,
  type MirriHostIdentity,
  type ManagedMirriOAuthRef,
} from '@mirri-ai/mirri-code-oauth';
import type {
  ProviderConfig as KosongProviderConfig,
  ProviderRequestAuth,
} from '@mirri-ai/kosong';
import { APIStatusError, UNKNOWN_CAPABILITY } from '@mirri-ai/kosong';

import { mapOAuthTokenError } from '#/oauth-error';

export interface MirriForCodingProviderOptions extends MirriHostIdentity {
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
  private readonly toolkit: MirriOAuthToolkit;
  private readonly homeDir: string;
  private readonly identity: MirriHostIdentity;
  private readonly oauthRef: ManagedMirriOAuthRef;

  constructor(options: MirriForCodingProviderOptions) {
    this.model = options.model ?? 'kimi-for-coding';
    this.baseUrl = options.baseUrl ?? mirriCodeBaseUrl();
    this.promptCacheKey = options.promptCacheKey;
    this.defaultHeaders = options.defaultHeaders;
    this.homeDir = resolveMirriHome(options.homeDir);
    this.identity = {
      userAgentProduct: options.userAgentProduct,
      version: options.version,
      userAgentSuffix: options.userAgentSuffix,
    };
    this.oauthRef = resolveMirriCodeOAuthRef({
      oauthHost: MIRRICODE_FLOW_CONFIG.oauthHost,
      baseUrl: this.baseUrl,
    });
    this.toolkit = new MirriOAuthToolkit({
      homeDir: this.homeDir,
      identity: this.identity,
    });
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
      let auth = await this.buildAuth(false);
      for (let refreshed = false; ; refreshed = true) {
        try {
          return await request(auth);
        } catch (error) {
          const is401 = error instanceof APIStatusError && error.statusCode === 401;
          if (!is401) throw error;
          if (refreshed) {
            throw new MirriError(
              ErrorCodes.AUTH_LOGIN_REQUIRED,
              'OAuth token was rejected after refresh. Run /login to re-authenticate.',
              { cause: error },
            );
          }
          auth = await this.buildAuth(true);
        }
      }
    };
  }

  private async buildAuth(force: boolean): Promise<ProviderRequestAuth> {
    try {
      const apiKey = await this.toolkit.ensureFresh(MIRRICODE_PROVIDER_NAME, {
        force,
        oauthRef: this.oauthRef,
      });
      return { apiKey };
    } catch (error) {
      throw mapOAuthTokenError(error, MIRRICODE_PROVIDER_NAME) ?? error;
    }
  }
}
