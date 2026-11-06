// apps/mirri-web/src/lib/modelPickerRefresh.ts
// The model-picker open flow's availability policy, extracted as pure
// logic (#183/#185): the picker opens on the already-cached model list
// and never blocks on the network refresh; the background refresh's
// failure surfaces through useModelProviderState's pushOperationFailure
// (its own loadModels fold and toast assembly) — nothing here needs to
// interpret that outcome.

/**
 * Given the picker's cached model count and the load state, compute the
 * `unavailable` flag the picker renders: the local list is the picker's
 * only data source, so "unavailable" is exactly "the local load failed
 * AND nothing is cached" — a background refresh in flight (or its
 * failure) never marks the picker unavailable (#183: the loading state
 * is owned by the local load, not the network refresh).
 */
export function modelPickerUnavailable(localLoadFailed: boolean, cachedCount: number): boolean {
  return localLoadFailed && cachedCount === 0;
}
