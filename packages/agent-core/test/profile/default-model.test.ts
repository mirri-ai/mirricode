import { describe, expect, it } from 'vitest';

import { resolveAgentProfiles } from '../../src/profile/resolve';

describe('profile defaultModel', () => {
  it('should resolve defaultModel through extends inheritance', () => {
    const resolved = resolveAgentProfiles([
      { name: 'base', defaultModel: 'claude-sonnet' },
      { name: 'child', extends: 'base' },
    ]);
    expect(resolved['child']?.defaultModel).toBe('claude-sonnet');
  });

  it('should allow child to override parent defaultModel', () => {
    const resolved = resolveAgentProfiles([
      { name: 'base', defaultModel: 'claude-sonnet' },
      { name: 'child', extends: 'base', defaultModel: 'gpt-4o' },
    ]);
    expect(resolved['child']?.defaultModel).toBe('gpt-4o');
  });

  it('should leave defaultModel undefined when not specified', () => {
    const resolved = resolveAgentProfiles([{ name: 'agent' }]);
    expect(resolved['agent']?.defaultModel).toBeUndefined();
  });
});
