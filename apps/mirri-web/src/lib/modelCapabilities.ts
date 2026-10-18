// apps/mirri-web/src/lib/modelCapabilities.ts
// Capability-badge rendering data for the Add-Provider dialog (G-25): the
// catalog model preview and the custom model editor show capability facts as
// badges whose labels come from i18n (`settings.models.capability.*`) — never
// hardcoded English. Known capability tags map to localized labels; unknown
// tags fall back to a generic "capability" label so non-preset capabilities
// (e.g. `video_in`, `dynamically_loaded_tools`) stay visible as disabled
// badges instead of disappearing. The vocabulary mirrors the ADR-004 facts
// the catalog service derives from models.dev
// (`image_in`/`video_in`/`audio_in`/`thinking`/`tool_use`/
// `dynamically_loaded_tools`).

/** The interactive capability chips the custom editor offers. */
export const CAPABILITY_PRESETS = ['image_in', 'thinking', 'tool_use'] as const;

/** Known capability tags → i18n message key carrying the badge label. */
const CAPABILITY_LABEL_KEYS: Record<string, string> = {
  image_in: 'settings.models.capability.imageIn',
  video_in: 'settings.models.capability.videoIn',
  audio_in: 'settings.models.capability.audioIn',
  thinking: 'settings.models.capability.thinking',
  tool_use: 'settings.models.capability.toolUse',
  dynamically_loaded_tools: 'settings.models.capability.dynamicTools',
};

/** The i18n message key labelling `cap`, or the generic fallback key for
 *  unknown/custom tags. */
export function capabilityLabelKey(cap: string): string {
  return CAPABILITY_LABEL_KEYS[cap] ?? 'settings.models.capability.unknown';
}

/** One capability badge row: `label` is an i18n message key; `disabled`
 *  marks capabilities outside the interactive presets. */
export interface ModelCapabilityBadge {
  key: string;
  label: string;
  disabled: boolean;
}

/** Map a model's capability facts (the catalog `capabilities` array plus the
 *  separate `reasoning` fact) to badge rows. Preset capabilities render
 *  enabled (interactive chips in the editor / regular badges in the preview);
 *  everything else renders as a disabled badge so no declared capability
 *  disappears. `thinking` is emitted when either the fact or the array
 *  declares it (never duplicated). `caps` may be undefined — models without
 *  declared facts produce no badges. */
export function modelCapabilityBadges(
  caps: string[] | undefined,
  reasoning: boolean,
): ModelCapabilityBadge[] {
  const presets: readonly string[] = CAPABILITY_PRESETS;
  const present = new Set(caps ?? []);
  if (reasoning) present.add('thinking');
  const ordered: string[] = presets.filter((cap) => present.has(cap));
  for (const cap of caps ?? []) {
    if (presets.includes(cap)) continue;
    if (ordered.includes(cap)) continue;
    ordered.push(cap);
  }
  return ordered.map((cap) => ({
    key: cap,
    label: capabilityLabelKey(cap),
    disabled: !presets.includes(cap),
  }));
}