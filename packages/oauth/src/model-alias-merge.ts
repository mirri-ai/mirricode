import { isRecord } from './utils';

/**
 * Read the `removedModelIds` denylist from a provider config. Models on this
 * list were deleted by the user and must not be re-appended by a refresh.
 */
export function readRemovedModelIds(provider: unknown): Set<string> {
  if (!isRecord(provider)) return new Set();
  const ids = provider['removedModelIds'];
  if (!Array.isArray(ids)) return new Set();
  return new Set(ids.filter((id): id is string => typeof id === 'string'));
}
