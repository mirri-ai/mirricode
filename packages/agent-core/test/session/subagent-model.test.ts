import { describe, expect, it } from 'vitest';

import type { ResolvedAgentProfile } from '../../src/profile';

/**
 * Tests for the model resolution precedence used by configureChild.
 *
 * Resolution order: explicit model param → profile.defaultModel → parent.config.modelAlias
 *
 * These tests verify the pure resolution logic before it is wired
 * into the actual configureChild method in subagent-host.ts.
 */
describe('subagent model resolution precedence', () => {
  it('should use profile.defaultModel when set and no explicit model', () => {
    const profile = { defaultModel: 'gpt-4o' } as Partial<ResolvedAgentProfile>;
    const parentModelAlias = 'claude-sonnet';
    const explicitModel: string | undefined = undefined;

    const resolved = explicitModel ?? profile.defaultModel ?? parentModelAlias;
    expect(resolved).toBe('gpt-4o');
  });

  it('should inherit parent model when profile has no defaultModel', () => {
    const profile = {} as Partial<ResolvedAgentProfile>;
    const parentModelAlias = 'claude-sonnet';
    const explicitModel: string | undefined = undefined;

    const resolved = explicitModel ?? profile.defaultModel ?? parentModelAlias;
    expect(resolved).toBe('claude-sonnet');
  });

  it('should prefer explicit model over profile.defaultModel and parent model', () => {
    const profile = { defaultModel: 'gpt-4o' } as Partial<ResolvedAgentProfile>;
    const parentModelAlias = 'claude-sonnet';
    const explicitModel = 'claude-haiku';

    const resolved = explicitModel ?? profile.defaultModel ?? parentModelAlias;
    expect(resolved).toBe('claude-haiku');
  });

  it('should inherit parent model when profile.defaultModel is undefined and no explicit model', () => {
    const profile = { defaultModel: undefined } as Partial<ResolvedAgentProfile>;
    const parentModelAlias = 'claude-sonnet';
    const explicitModel: string | undefined = undefined;

    const resolved = explicitModel ?? profile.defaultModel ?? parentModelAlias;
    expect(resolved).toBe('claude-sonnet');
  });
});
