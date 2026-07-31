// apps/mirri-web/src/lib/resolveAlias.ts
// Returns the alias as-is. The former overrides merge layer was removed when
// catalog refreshes stopped modifying existing alias fields — what is stored
// is now exactly what the runtime uses. Kept as a thin passthrough so callers
// do not need to change.

import type { AppModelAlias } from '../api/types';

export function resolveAlias(alias: AppModelAlias): AppModelAlias {
  return alias;
}
