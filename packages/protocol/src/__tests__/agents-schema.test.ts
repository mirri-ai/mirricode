import { describe, expect, it } from 'vitest';

import {
  profileEntrySchema,
  listProfilesResponseSchema,
  createProfileRequestSchema,
  updateProfileRequestSchema,
  configResponseSchema,
  patchConfigRequestSchema,
} from '../../src';

describe('agent profile protocol schemas', () => {
  it('should validate a complete profileEntry', () => {
    const valid = {
      name: 'reviewer',
      source: 'user',
      builtin: false,
      essential: false,
      enabled: true,
      has_override: false,
    };
    expect(profileEntrySchema.safeParse(valid).success).toBe(true);
  });

  it('should validate a builtin profileEntry', () => {
    const valid = {
      name: 'coder',
      source: 'builtin',
      builtin: true,
      essential: false,
      enabled: true,
      has_override: false,
    };
    expect(profileEntrySchema.safeParse(valid).success).toBe(true);
  });

  it('should validate an essential profileEntry', () => {
    const valid = {
      name: 'agent',
      source: 'builtin',
      builtin: true,
      essential: true,
      enabled: true,
      has_override: false,
    };
    expect(profileEntrySchema.safeParse(valid).success).toBe(true);
  });

  it('should validate createProfileRequest with minimal fields', () => {
    const valid = { name: 'reviewer', extends: 'agent' };
    expect(createProfileRequestSchema.safeParse(valid).success).toBe(true);
  });

  it('should reject createProfileRequest without name', () => {
    expect(createProfileRequestSchema.safeParse({}).success).toBe(false);
  });

  it('should validate updateProfileRequest as partial', () => {
    const valid = { description: 'Updated description' };
    expect(updateProfileRequestSchema.safeParse(valid).success).toBe(true);
  });

  it('should validate listProfilesResponse', () => {
    const valid = {
      items: [
        { name: 'agent', source: 'builtin', builtin: true, essential: true, enabled: true, has_override: false },
      ],
    };
    expect(listProfilesResponseSchema.safeParse(valid).success).toBe(true);
  });

  it('should accept extra_agent_dirs in configResponse', () => {
    const valid = {
      providers: {},
      extra_agent_dirs: ['/custom/agents'],
      disabled_agents: ['explore'],
    };
    expect(configResponseSchema.safeParse(valid).success).toBe(true);
  });

  it('should accept extra_agent_dirs in patchConfigRequest', () => {
    const valid = {
      extra_agent_dirs: ['/custom/agents'],
      disabled_agents: ['explore'],
    };
    expect(patchConfigRequestSchema.safeParse(valid).success).toBe(true);
  });
});
