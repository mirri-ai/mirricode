// apps/mirri-web/src/lib/providerBaseUrl.test.ts
// Base-URL edit logic for the provider detail pane (#187): the pure
// decision helpers — payload construction and clear-confirmation gating —
// extracted from ProviderDetail.vue so they are unit-testable without a
// component harness (the repo has no @vue/test-utils; the
// modelProviderCreateGuide precedent).

import { describe, expect, it } from 'vitest';

import type { AppProvider } from '../api/types';
import { buildBaseUrlConfigPatch, clearingBaseUrlNeedsConfirm } from './providerBaseUrl';

function provider(partial: Partial<AppProvider> & { id: string }): AppProvider {
  return {
    type: 'openai',
    hasApiKey: true,
    status: 'connected',
    ...partial,
  };
}

describe('buildBaseUrlConfigPatch', () => {
  it('should build a providers patch carrying the trimmed base url when a value is entered', () => {
    // Given a configured provider and an edited base URL with surrounding
    // whitespace
    const p = provider({ id: 'acme' });

    // When the config patch is built
    const patch = buildBaseUrlConfigPatch(p, '  https://gw.example.test/v1  ');

    // Then the patch addresses the provider with the trimmed URL
    expect(patch).toEqual({
      providers: { acme: { baseUrl: 'https://gw.example.test/v1' } },
    });
  });

  it('should build an empty-string baseUrl patch when the value is cleared', () => {
    // Given a provider whose base URL is being removed (an empty string is
    // the clear encoding — the engine treats "" as unset, restoring the
    // first-party default)
    const p = provider({ id: 'acme' });

    // When the config patch is built from an empty input
    const patch = buildBaseUrlConfigPatch(p, '   ');

    // Then the patch carries "" so the engine resolves the URL as unset
    expect(patch).toEqual({
      providers: { acme: { baseUrl: '' } },
    });
  });
});

describe('clearingBaseUrlNeedsConfirm', () => {
  it('should not require confirmation when a first-party provider is reset to its default', () => {
    // Given the official openai provider and an empty edit value (blank
    // restores the default endpoint)
    const p = provider({ id: 'openai' });

    // When the confirmation gate is evaluated
    // Then no confirmation is needed (the default answers)
    expect(clearingBaseUrlNeedsConfirm(p, '')).toBe(false);
  });

  it('should require confirmation when a third-party provider would be left without a base url', () => {
    // Given a third-party provider and an empty edit value (clearing leaves
    // the provider without an endpoint — the engine would then refuse to
    // build it, #184)
    const p = provider({ id: 'acme' });

    // When the confirmation gate is evaluated
    // Then the clear needs a confirm dialog
    expect(clearingBaseUrlNeedsConfirm(p, '')).toBe(true);
  });

  it('should not require confirmation when the value is a non-empty edit', () => {
    // Given any provider and a non-empty edit
    const p = provider({ id: 'acme' });

    // When the confirmation gate is evaluated
    // Then a normal save needs no confirmation
    expect(clearingBaseUrlNeedsConfirm(p, 'https://gw.example.test/v1')).toBe(false);
  });
});
