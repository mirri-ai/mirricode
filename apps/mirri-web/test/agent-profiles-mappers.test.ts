import { describe, expect, it } from 'vitest';

import type { WireAgentProfile } from '../src/api/daemon/wire';
import type { AppAgentProfile } from '../src/api/types';
import { toAppAgentProfileList, toAppProfile } from '../src/api/daemon/mappers';

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

  it('should map tools fields', () => {
    const wire: WireAgentProfile = {
      name: 'reviewer',
      source: 'user',
      builtin: false,
      essential: false,
      enabled: true,
      tools: ['Read', 'Grep'],
    };
    const app = toAppProfile(wire);
    expect(app.tools).toEqual(['Read', 'Grep']);
  });

  it('should map suppressed candidates, skipped files, and the new detail fields from the wire', () => {
    const mapped = toAppProfile({
      name: 'reviewer', source: 'user', builtin: false, essential: false,
      enabled: false, has_override: true,
      suppressed_candidates: [{ source: 'project', priority: 30, reason: 'disabled' }],
      disallowed_tools: ['Bash'],
      subagents: ['coder'],
    });
    expect(mapped.suppressedCandidates).toEqual([
      { source: 'project', priority: 30, reason: 'disabled' },
    ]);
    expect(mapped.disallowedTools).toEqual(['Bash']);
    expect(mapped.subagents).toEqual(['coder']);
  });

  it('should map capabilities_required from the wire (A2-3 display side)', () => {
    const mapped = toAppProfile({
      name: 'explore', source: 'builtin', builtin: true, essential: false, enabled: true,
      capabilities_required: ['code.explore', 'code.read', 'web.search', 'web.fetch'],
    });
    expect(mapped.capabilitiesRequired).toEqual([
      'code.explore', 'code.read', 'web.search', 'web.fetch',
    ]);
  });

  it('should leave capabilitiesRequired undefined when the wire omits it', () => {
    const mapped = toAppProfile({
      name: 'coder', source: 'builtin', builtin: true, essential: false, enabled: true,
    });
    expect(mapped.capabilitiesRequired).toBeUndefined();
  });

  it('should map all six source kinds through without collapsing them', () => {
    const sources = ['builtin', 'user', 'project', 'extra', 'plugin', 'explicit'] as const;
    for (const source of sources) {
      const mapped = toAppProfile({
        name: 'x', source, builtin: false, essential: false, enabled: true,
      });
      expect(mapped.source).toBe(source);
    }
  });

  it('should map the agent list response with skipped files', () => {
    const result = toAppAgentProfileList({
      items: [{ name: 'x', source: 'user', builtin: false, essential: false, enabled: true }],
      skipped: [{ path: '/agents/broken.md', reason: 'missing description' }],
    });
    expect(result.profiles).toHaveLength(1);
    expect(result.profiles[0]!.name).toBe('x');
    expect(result.skippedAgentFiles).toEqual([
      { path: '/agents/broken.md', reason: 'missing description' },
    ]);
  });
});