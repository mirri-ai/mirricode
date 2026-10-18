import {
  refreshProviderModels,
  type ProviderChange,
  type RefreshProviderOptions,
  type RefreshResult,
} from '@mirri-ai/kosong';
import type { MirriConfig, MirriConfigPatch } from '@mirri-ai/mirri-code-sdk';

/**
 * CLI-side host for provider-model refresh. Kept on the SDK's full config types
 * so existing TUI callers (and tests) don't change; the kosong orchestrator
 * (open platforms + custom registries; managed OAuth refresh is retired)
 * works against its minimal `ProviderConfigShape`.
 */
export interface RefreshProviderHost {
  getConfig(): Promise<MirriConfig>;
  removeProvider(providerId: string): Promise<MirriConfig>;
  setConfig(patch: MirriConfigPatch): Promise<MirriConfig>;
}

export type { ProviderChange, RefreshProviderOptions, RefreshResult };

/**
 * Refresh remote model metadata for the configured providers. Thin adapter over
 * the shared `refreshProviderModels` orchestrator in `@mirri-ai/kosong` (which
 * is also what the daemon's scheduled/manual refresh uses).
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
    },
    options,
  );
}