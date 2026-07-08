import {
  refreshProviderModels,
  type ProviderChange,
  type RefreshProviderOptions,
  type RefreshProviderScope,
  type RefreshResult,
} from '@mirri-ai/mirri-code-oauth';
import type { MirriConfig, MirriConfigPatch, OAuthRef } from '@mirri-ai/mirri-code-sdk';

/**
 * CLI-side host for provider-model refresh. Kept on the SDK's full config types
 * so existing TUI callers (and tests) don't change; the daemon uses the oauth
 * package's `ManagedMirriConfigShape`-typed host directly.
 */
export interface RefreshProviderHost {
  getConfig(): Promise<MirriConfig>;
  removeProvider(providerId: string): Promise<MirriConfig>;
  setConfig(patch: MirriConfigPatch): Promise<MirriConfig>;
  resolveOAuthToken(providerName: string, oauthRef?: OAuthRef): Promise<string>;
}

export type { ProviderChange, RefreshProviderOptions, RefreshProviderScope, RefreshResult };

/**
 * Refresh remote model metadata for the configured providers. Thin adapter over
 * the shared `refreshProviderModels` orchestrator in `@mirri-ai/mirri-code-oauth`
 * (which is also what the daemon's scheduled/manual refresh uses).
 */
export async function refreshAllProviderModels(
  host: RefreshProviderHost,
  options: RefreshProviderOptions = {},
): Promise<RefreshResult> {
  return refreshProviderModels(
    {
      getConfig: () => host.getConfig(),
      removeProvider: (providerId) => host.removeProvider(providerId),
      setConfig: (patch) => host.setConfig(patch as unknown as MirriConfigPatch),
      resolveOAuthToken: (providerName, oauthRef) =>
        host.resolveOAuthToken(providerName, oauthRef as unknown as OAuthRef),
    },
    options,
  );
}
