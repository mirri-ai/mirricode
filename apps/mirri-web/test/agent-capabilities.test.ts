// apps/mirri-web/test/agent-capabilities.test.ts
// Capability-tag display mapping (A2-3): known tags resolve to localized
// description keys; unknown/custom tags resolve to undefined so the UI
// renders the raw tag alone. Also guards zh/en locale parity for the
// description keys — the app has no automated missing-key check.

import { describe, expect, it } from 'vitest';

import { capabilityDescKey } from '../src/lib/agentCapabilities';
import enAgents from '../src/i18n/locales/en/agents';
import zhAgents from '../src/i18n/locales/zh/agents';

describe('capabilityDescKey (known tag → i18n description key)', () => {
  it('should map the four tags the built-in tools declare', () => {
    // The known set mirrors docs/en/customization/integrations.md
    // "Available capabilities" — the only tags with built-in providers.
    expect(capabilityDescKey('code.explore')).toBe('agents.capabilityDesc.codeExplore');
    expect(capabilityDescKey('code.read')).toBe('agents.capabilityDesc.codeRead');
    expect(capabilityDescKey('web.search')).toBe('agents.capabilityDesc.webSearch');
    expect(capabilityDescKey('web.fetch')).toBe('agents.capabilityDesc.webFetch');
  });

  it('should return undefined for unknown/custom tags (render raw, no description)', () => {
    // The registry accepts any non-empty tag; a missing entry must not
    // produce an empty string description in the UI.
    expect(capabilityDescKey('code.write')).toBeUndefined();
    expect(capabilityDescKey('mcp.custom')).toBeUndefined();
    expect(capabilityDescKey('')).toBeUndefined();
  });
});

describe('capability description locale parity (zh/en)', () => {
  const KNOWN_TAGS = ['code.explore', 'code.read', 'web.search', 'web.fetch'];

  it('should have a non-empty description for every known tag in both locales', () => {
    for (const tag of KNOWN_TAGS) {
      const key = capabilityDescKey(tag)?.split('.').pop();
      expect(key).toBeDefined();
      const enDesc = (enAgents.capabilityDesc as Record<string, string>)[key!];
      const zhDesc = (zhAgents.capabilityDesc as Record<string, string>)[key!];
      expect(typeof enDesc).toBe('string');
      expect(enDesc.length).toBeGreaterThan(0);
      expect(typeof zhDesc).toBe('string');
      expect(zhDesc.length).toBeGreaterThan(0);
    }
  });

  it('should label the capabilities section in both locales', () => {
    expect(enAgents.capabilities).toBeTruthy();
    expect(zhAgents.capabilities).toBeTruthy();
    expect(enAgents.disallowedToolsHint).toBeTruthy();
    expect(zhAgents.disallowedToolsHint).toBeTruthy();
  });
});
