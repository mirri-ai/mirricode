import { describe, expect, it } from 'vitest';

import type { WireAgentProfile } from '../src/api/daemon/wire';
import type { AppAgentProfile } from '../src/api/types';
import { toAppProfile } from '../src/api/daemon/mappers';

describe('agent profile wire↔app mappers', () => {
  it('should map wire profile to app profile (snake→camel)', () => {
    const wire: WireAgentProfile = {
      name: 'reviewer',
      source: 'user',
      builtin: false,
      essential: false,
      enabled: true,
      default_model: 'gpt-4o',
      when_to_use: 'Use for code review',
      file_path: '/home/.mirri-code/agents/reviewer.yaml',
    };
    const app = toAppProfile(wire);
    expect(app.name).toBe('reviewer');
    expect(app.source).toBe('user');
    expect(app.builtin).toBe(false);
    expect(app.essential).toBe(false);
    expect(app.enabled).toBe(true);
    expect(app.defaultModel).toBe('gpt-4o');
    expect(app.whenToUse).toBe('Use for code review');
    expect(app.filePath).toBe('/home/.mirri-code/agents/reviewer.yaml');
  });

  it('should map builtin profile with minimal fields', () => {
    const wire: WireAgentProfile = {
      name: 'coder',
      source: 'builtin',
      builtin: true,
      essential: false,
      enabled: true,
    };
    const app = toAppProfile(wire);
    expect(app.name).toBe('coder');
    expect(app.builtin).toBe(true);
    expect(app.defaultModel).toBeUndefined();
    expect(app.filePath).toBeUndefined();
  });

  it('should map essential agent profile', () => {
    const wire: WireAgentProfile = {
      name: 'agent',
      source: 'builtin',
      builtin: true,
      essential: true,
      enabled: true,
    };
    const app = toAppProfile(wire);
    expect(app.essential).toBe(true);
    expect(app.enabled).toBe(true);
  });

  it('should map disabled profile', () => {
    const wire: WireAgentProfile = {
      name: 'explore',
      source: 'builtin',
      builtin: true,
      essential: false,
      enabled: false,
    };
    const app = toAppProfile(wire);
    expect(app.enabled).toBe(false);
  });

  it('should map tools and extends fields', () => {
    const wire: WireAgentProfile = {
      name: 'reviewer',
      source: 'user',
      builtin: false,
      essential: false,
      enabled: true,
      extends: 'agent',
      tools: ['Read', 'Grep'],
    };
    const app = toAppProfile(wire);
    expect(app.extends).toBe('agent');
    expect(app.tools).toEqual(['Read', 'Grep']);
  });
});

// ---------------------------------------------------------------------------
// "Create from X" — self-contained profile data transformation
// Mirrors the startFrom prefill + submit logic in AgentProfilesPanel.vue.
// ---------------------------------------------------------------------------

/**
 * Builds a self-contained create-agent payload from a base profile, mirroring
 * the "Start from" affordance in the web UI. All fields are prefilled from
 * the base, the name is replaced, and `extends` is never included — the
 * stored profile is flat / self-contained.
 */
function buildCreateFromBase(
  name: string,
  base: AppAgentProfile,
): Record<string, unknown> {
  const tools = base.tools?.length ? base.tools : undefined;
  const promptVars = base.promptVars && Object.keys(base.promptVars).length > 0
    ? base.promptVars
    : undefined;

  return {
    name,
    description: base.description || undefined,
    // NO `extends` — self-contained profile
    defaultModel: base.defaultModel || undefined,
    tools,
    whenToUse: base.whenToUse || undefined,
    systemPromptTemplate: base.systemPromptTemplate || undefined,
    promptVars,
  };
}

describe('create profile from base profile (self-contained, no extends)', () => {
  const coderProfile: AppAgentProfile = {
    name: 'coder',
    source: 'builtin',
    builtin: true,
    essential: false,
    enabled: true,
    hasOverride: false,
    description: 'General coding agent',
    defaultModel: 'claude-sonnet-4',
    tools: ['Read', 'Write', 'Edit', 'Bash', 'Grep', 'Glob'],
    whenToUse: 'Use for general coding tasks',
    systemPromptTemplate: 'You are a coding assistant.',
    promptVars: { roleAdditional: 'Focus on clean code.' },
  };

  it('should prefill all fields from the base profile', () => {
    const result = buildCreateFromBase('my-coder', coderProfile);
    expect(result.name).toBe('my-coder');
    expect(result.description).toBe('General coding agent');
    expect(result.defaultModel).toBe('claude-sonnet-4');
    expect(result.tools).toEqual(['Read', 'Write', 'Edit', 'Bash', 'Grep', 'Glob']);
    expect(result.whenToUse).toBe('Use for general coding tasks');
    expect(result.systemPromptTemplate).toBe('You are a coding assistant.');
    expect(result.promptVars).toEqual({ roleAdditional: 'Focus on clean code.' });
  });

  it('should not include extends in the create payload', () => {
    const result = buildCreateFromBase('my-coder', coderProfile);
    expect(result).not.toHaveProperty('extends');
  });

  it('should produce a self-contained profile even when base has extends', () => {
    const baseWithExtends: AppAgentProfile = {
      ...coderProfile,
      name: 'specialist',
      extends: 'coder',
    };
    const result = buildCreateFromBase('my-specialist', baseWithExtends);
    expect(result).not.toHaveProperty('extends');
    // Fields are still prefilled from the base
    expect(result.tools).toEqual(['Read', 'Write', 'Edit', 'Bash', 'Grep', 'Glob']);
  });

  it('should omit empty optional fields when base has no values', () => {
    const minimalBase: AppAgentProfile = {
      name: 'agent',
      source: 'builtin',
      builtin: true,
      essential: true,
      enabled: true,
      hasOverride: false,
    };
    const result = buildCreateFromBase('my-agent', minimalBase);
    expect(result.name).toBe('my-agent');
    expect(result.description).toBeUndefined();
    expect(result.defaultModel).toBeUndefined();
    expect(result.tools).toBeUndefined();
    expect(result.whenToUse).toBeUndefined();
    expect(result.systemPromptTemplate).toBeUndefined();
    expect(result.promptVars).toBeUndefined();
  });
});
