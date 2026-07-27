// apps/mirri-web/src/lib/resolveAlias.ts
// Resolve model alias overrides into top-level fields so the UI always
// displays effective values — what the runtime actually uses. Mirrors the
// agent-core `effectiveModelAlias` merge: `{ ...base, ...overrides }`.

import type { AppModelAlias } from '../api/types';

export function resolveAlias(alias: AppModelAlias): AppModelAlias {
  const ov = alias.overrides;
  if (!ov) return alias;
  // Strip `overrides` from the spread — the resolved object should only carry
  // effective top-level values, not the raw overrides sub-object. Without this,
  // buildPatch's baseline (`originalAlias`) would contain both the resolved
  // field and the overrides copy, which is confusing and can cause stale
  // override fields to accumulate in the config via deepMerge (which only
  // adds, never removes).
  const { overrides: _ov, ...base } = alias;
  return {
    ...base,
    // Overridable fields — override wins when present
    displayName: ov.displayName ?? alias.displayName,
    maxContextSize: ov.maxContextSize ?? alias.maxContextSize,
    maxOutputSize: ov.maxOutputSize ?? alias.maxOutputSize,
    capabilities: ov.capabilities ?? alias.capabilities,
    supportEfforts: ov.supportEfforts ?? alias.supportEfforts,
    defaultEffort: ov.defaultEffort ?? alias.defaultEffort,
    reasoningKey: ov.reasoningKey ?? alias.reasoningKey,
    adaptiveThinking: ov.adaptiveThinking ?? alias.adaptiveThinking,
    description: ov.description ?? alias.description,
    // protocol and betaApi are NOT overridable — always top-level
  };
}
