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
