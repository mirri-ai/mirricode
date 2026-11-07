// apps/mirri-web/src/lib/thinkingOff.ts
// The "thinking off spelling" field: which wire field this endpoint reads to
// stop extended reasoning. An implementation detail of ours, not a model
// fact — so the empty value means "let the engine infer it", and the UI never
// pretends the user has to understand the inference to trust it.

/** The selectable values, in menu order. The empty string is "auto". */
export const THINKING_OFF_OPTIONS = [
  { value: '', labelKey: 'settings.models.thinkingOff.auto' },
  { value: 'thinking_type', labelKey: 'settings.models.thinkingOff.thinkingType' },
  { value: 'reasoning_effort', labelKey: 'settings.models.thinkingOff.reasoningEffort' },
  { value: 'enable_thinking', labelKey: 'settings.models.thinkingOff.enableThinking' },
] as const;

const KNOWN: Record<string, string> = {
  thinking_type: 'settings.models.thinkingOff.thinkingType',
  reasoning_effort: 'settings.models.thinkingOff.reasoningEffort',
  enable_thinking: 'settings.models.thinkingOff.enableThinking',
};

/** The i18n key labelling `value`; empty/unknown falls back to "auto". */
export function thinkingOffLabelKey(value: string | undefined): string {
  if (value === undefined || value.trim() === '') {
    return 'settings.models.thinkingOff.auto';
  }
  return KNOWN[value.trim()] ?? 'settings.models.thinkingOff.auto';
}

const INFERRED: Record<string, string> = {
  thinking_type: 'settings.models.thinkingOff.autoResolved.thinkingType',
  reasoning_effort: 'settings.models.thinkingOff.autoResolved.reasoningEffort',
  enable_thinking: 'settings.models.thinkingOff.autoResolved.enableThinking',
};

/**
 * The hint line under the select: for an explicit value it names that value;
 * for "auto" it names what the engine inferred, or says the endpoint declares
 * nothing (in which case the composer hides the off segment).
 */
export function describeThinkingOff(
  value: string | undefined,
  inferred: string | undefined,
): string {
  if (value !== undefined && value.trim() !== '') {
    return thinkingOffLabelKey(value);
  }
  if (inferred !== undefined && inferred.trim() !== '') {
    return INFERRED[inferred.trim()] ?? 'settings.models.thinkingOff.autoUnresolved';
  }
  return 'settings.models.thinkingOff.autoUnresolved';
}
