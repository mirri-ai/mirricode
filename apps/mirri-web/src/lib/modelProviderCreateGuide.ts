// apps/mirri-web/src/lib/modelProviderCreateGuide.ts
// First-run guidance after creating a provider: the UI selects the new
// provider in the detail pane and — when the provider has no models yet —
// auto-opens the add-model flow. Pure decision helpers extracted from
// ModelsPanel / ProviderDetail so the interaction is unit-testable without a
// component harness (this repo has no @vue/test-utils).

import type { AppModelAlias, AppProvider } from '../api/types';

/** Count model aliases pointing at the given provider. */
export function modelCountForProvider(
  providerId: string,
  models: Readonly<Record<string, AppModelAlias>>,
): number {
  return Object.values(models).filter((m) => m.provider === providerId).length;
}

/** The provider id that should be selected and guided after a create, or
 *  undefined when there is nothing to guide:
 *  - no provider id was reported by the create (the catalog import path —
 *    it may merge into an existing provider and always ships models), or
 *  - the created id is missing from the reloaded provider list (create
 *    failed or was superseded), or
 *  - the provider already has models — there is nothing to add. */
export function providerToGuideAfterCreate(params: {
  createdProviderId?: string;
  providers: ReadonlyArray<Pick<AppProvider, 'id'>>;
  models: Readonly<Record<string, AppModelAlias>>;
}): string | undefined {
  const { createdProviderId, providers, models } = params;
  if (!createdProviderId) return undefined;
  if (!providers.some((p) => p.id === createdProviderId)) return undefined;
  if (modelCountForProvider(createdProviderId, models) > 0) return undefined;
  return createdProviderId;
}
