// apps/mirri-web/src/lib/modelProviderCreateGuide.test.ts
// Pure-logic tests for the first-run guidance after a provider create
// (ModelsPanel → ProviderDetail): the decision that a freshly created
// provider must be selected and its add-model dialog auto-opened, guarded to
// zero-model providers only. Extracted into `modelProviderCreateGuide.ts`
// because this repo has no component test harness (no @vue/test-utils) — the
// same pattern as the agent-profiles panel-logic tests.

import { describe, expect, it } from 'vitest';

import type { AppModelAlias, AppProvider } from '../api/types';
import { modelCountForProvider, providerToGuideAfterCreate } from './modelProviderCreateGuide';

function provider(partial: Partial<AppProvider> & { id: string }): AppProvider {
  return {
    type: 'openai',
    hasApiKey: true,
    status: 'connected',
    ...partial,
  };
}

function alias(providerId: string, model: string): AppModelAlias {
  return { provider: providerId, model, maxContextSize: 128000 };
}

describe('providerToGuideAfterCreate', () => {
  it('should select the created provider when it has zero models', () => {
    // Given a custom-mode create that reported an id and the reloaded
    // provider list contains it with no model aliases yet
    const providers = [provider({ id: 'acme' })];

    // When the guidance decision is computed
    const target = providerToGuideAfterCreate({
      createdProviderId: 'acme',
      providers,
      models: {},
    });

    // Then the provider is returned for selection + add-model guidance
    expect(target).toBe('acme');
  });

  it('should not guide when the created provider already has models', () => {
    // Given a provider that shipped models with its create (catalog import)
    const providers = [provider({ id: 'acme' })];
    const models: Record<string, AppModelAlias> = {
      'acme/m-1': alias('acme', 'model-one'),
    };

    // When the guidance decision is computed
    const target = providerToGuideAfterCreate({
      createdProviderId: 'acme',
      providers,
      models,
    });

    // Then nothing is selected or guided — the provider needs no add-model
    expect(target).toBeUndefined();
  });

  it('should not guide when no create reported a provider id', () => {
    // Given a create that did not report an id (catalog import path)
    const providers = [provider({ id: 'acme' })];

    // When the guidance decision is computed without a created id
    const target = providerToGuideAfterCreate({ providers, models: {} });

    // Then nothing is guided
    expect(target).toBeUndefined();
  });

  it('should not guide when the created provider is missing from the reloaded list', () => {
    // Given a create reported an id but the reloaded list no longer
    // contains it (the create was superseded or failed)
    const providers = [provider({ id: 'other' })];

    // When the guidance decision is computed
    const target = providerToGuideAfterCreate({
      createdProviderId: 'acme',
      providers,
      models: {},
    });

    // Then nothing is guided — the selection would be invalid
    expect(target).toBeUndefined();
  });

  it('should ignore model aliases belonging to other providers', () => {
    // Given the created provider has no models but another provider does
    const providers = [provider({ id: 'acme' }), provider({ id: 'other' })];
    const models: Record<string, AppModelAlias> = {
      'other/m-1': alias('other', 'model-one'),
    };

    // When the guidance decision is computed
    const target = providerToGuideAfterCreate({
      createdProviderId: 'acme',
      providers,
      models,
    });

    // Then the empty provider is still guided
    expect(target).toBe('acme');
  });
});

describe('modelCountForProvider', () => {
  it('should count only aliases of the requested provider', () => {
    // Given aliases spread across two providers
    const models: Record<string, AppModelAlias> = {
      'acme/m-1': alias('acme', 'model-one'),
      'acme/m-2': alias('acme', 'model-two'),
      'other/m-1': alias('other', 'model-one'),
    };

    // When the count is computed per provider
    const acme = modelCountForProvider('acme', models);
    const other = modelCountForProvider('other', models);

    // Then each count only sees its own aliases
    expect(acme).toBe(2);
    expect(other).toBe(1);
  });

  it('should return zero when the provider has no aliases', () => {
    // Given no aliases at all
    // When the count is computed
    // Then it is zero
    expect(modelCountForProvider('acme', {})).toBe(0);
  });
});
