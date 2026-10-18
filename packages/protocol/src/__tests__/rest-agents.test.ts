import { describe, expect, it } from 'vitest';

import {
  createProfileRequestSchema,
  listProfilesResponseSchema,
  profileEntrySchema,
} from '../rest/agents';

describe('agent profile wire schemas', () => {
  it('should accept every documented source kind', () => {
    for (const source of ['builtin', 'user', 'project', 'extra', 'plugin', 'explicit']) {
      expect(() =>
        profileEntrySchema.parse({
          name: 'coder', source, builtin: false, essential: false,
          enabled: true, has_override: false,
        }),
      ).not.toThrow();
    }
  });

  it('should accept suppressed candidates, skipped files, and the new detail fields', () => {
    expect(() =>
      profileEntrySchema.parse({
        name: 'reviewer', source: 'user', builtin: false, essential: false,
        enabled: false, has_override: true,
        suppressed_candidates: [
          { source: 'project', priority: 30, reason: 'disabled' },
          { source: 'user', priority: 10, reason: 'priority' },
        ],
        disallowed_tools: ['Bash'],
        capabilities_required: ['code.read'],
        subagents: ['coder'],
      }),
    ).not.toThrow();

    expect(() =>
      listProfilesResponseSchema.parse({
        items: [],
        skipped: [{ path: '/home/agents/broken.md', reason: 'Missing required frontmatter field "description"' }],
      }),
    ).not.toThrow();
  });

  it('should reject an unknown suppressed reason', () => {
    expect(() =>
      profileEntrySchema.parse({
        name: 'x', source: 'user', builtin: false, essential: false,
        enabled: true, has_override: false,
        suppressed_candidates: [{ source: 'user', priority: 10, reason: 'mystery' }],
      }),
    ).toThrow();
  });

  it('should accept subagents and disallowed_tools on the create request', () => {
    expect(() =>
      createProfileRequestSchema.parse({
        name: 'reviewer', description: 'd',
        subagents: ['coder'], disallowed_tools: ['Bash'],
      }),
    ).not.toThrow();
  });
});
