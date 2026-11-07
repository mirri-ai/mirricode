import { describe, expect, it } from 'vitest';
import { thinkingCopyKey } from '../src/lib/modelThinking';

describe('thinking control copy', () => {
  it('should point at the always-thinking explanation when the model cannot be turned off', () => {
    expect(thinkingCopyKey({ capabilities: ['thinking', 'always_thinking'] })).toBe(
      'composer.thinkingAlwaysOn',
    );
  });

  it('should stay silent when the model can be turned off', () => {
    expect(thinkingCopyKey({ capabilities: ['thinking'], supportEfforts: ['low'] })).toBeNull();
  });

  it('should stay silent for a model that does not think at all', () => {
    expect(thinkingCopyKey({ capabilities: [] })).toBeNull();
  });
});
