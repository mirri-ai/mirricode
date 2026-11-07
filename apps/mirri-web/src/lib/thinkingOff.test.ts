import { describe, expect, it } from 'vitest';
import { THINKING_OFF_OPTIONS, describeThinkingOff, thinkingOffLabelKey } from './thinkingOff';

describe('thinkingOff', () => {
  it('should offer auto plus the three wire spellings when listing options', () => {
    expect(THINKING_OFF_OPTIONS.map((o) => o.value)).toEqual([
      '',
      'thinking_type',
      'reasoning_effort',
      'enable_thinking',
    ]);
  });

  it('should resolve a known value to its own label key when labelling', () => {
    expect(thinkingOffLabelKey('thinking_type')).toBe('settings.models.thinkingOff.thinkingType');
    expect(thinkingOffLabelKey('reasoning_effort')).toBe(
      'settings.models.thinkingOff.reasoningEffort',
    );
    expect(thinkingOffLabelKey('enable_thinking')).toBe(
      'settings.models.thinkingOff.enableThinking',
    );
  });

  it('should resolve auto when the value is empty or undefined', () => {
    expect(thinkingOffLabelKey('')).toBe('settings.models.thinkingOff.auto');
    expect(thinkingOffLabelKey(undefined)).toBe('settings.models.thinkingOff.auto');
  });

  it('should name the inferred spelling when the value is auto', () => {
    expect(describeThinkingOff('', 'thinking_type')).toBe(
      'settings.models.thinkingOff.autoResolved.thinkingType',
    );
  });

  it('should say the endpoint declares nothing when auto resolves to nothing', () => {
    expect(describeThinkingOff('', undefined)).toBe('settings.models.thinkingOff.autoUnresolved');
  });

  it('should describe the explicit value itself when one is set', () => {
    expect(describeThinkingOff('enable_thinking', 'thinking_type')).toBe(
      'settings.models.thinkingOff.enableThinking',
    );
  });
});
