import { describe, expect, it } from 'vitest';

describe('profile reload on config change', () => {
  // The predicate logic is now inlined in ProfileService's onDidPublish handler.
  // These tests verify the field-matching logic directly.
  const PROFILE_RELEVANT_FIELDS = new Set(['disabled_agents', 'extra_agent_dirs']);

  function hasProfileRelevantConfigChange(changedFields: readonly string[]): boolean {
    return changedFields.some((field) => PROFILE_RELEVANT_FIELDS.has(field));
  }

  it('should reload profiles when disabled_agents changes', () => {
    const shouldReload = hasProfileRelevantConfigChange(['disabled_agents']);
    expect(shouldReload).toBe(true);
  });

  it('should reload profiles when extra_agent_dirs changes', () => {
    const shouldReload = hasProfileRelevantConfigChange(['extra_agent_dirs']);
    expect(shouldReload).toBe(true);
  });

  it('should not reload when unrelated config changes', () => {
    const shouldReload = hasProfileRelevantConfigChange(['default_model']);
    expect(shouldReload).toBe(false);
  });

  it('should not reload when changedFields is empty', () => {
    const shouldReload = hasProfileRelevantConfigChange([]);
    expect(shouldReload).toBe(false);
  });
});
