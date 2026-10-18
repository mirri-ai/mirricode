// apps/mirri-web/src/lib/agentCapabilities.ts
// Capability-tag display helpers for agent profiles (A2-3). Known tags map
// to localized descriptions (zh/en, `agents.capabilityDesc.*`); unknown tags
// have no description — callers must render the raw tag alone rather than an
// empty string, because the registry accepts any non-empty custom tag.

/** Known capability tags → i18n message key carrying the description.
 * Kept honest against the tags the built-in tools actually declare
 * (docs/en/customization/integrations.md "Available capabilities"). */
const CAPABILITY_DESC_KEYS: Record<string, string> = {
  'code.explore': 'agents.capabilityDesc.codeExplore',
  'code.read': 'agents.capabilityDesc.codeRead',
  'web.search': 'agents.capabilityDesc.webSearch',
  'web.fetch': 'agents.capabilityDesc.webFetch',
};

/** The i18n key describing `tag`, or undefined for unknown/custom tags. */
export function capabilityDescKey(tag: string): string | undefined {
  return CAPABILITY_DESC_KEYS[tag];
}
