// apps/mirri-web/test/provider-credentials.test.ts
// Task 4/5 of the credential closeout — the settings page's credential
// logic as pure functions (the repo has no @vue/test-utils; the
// TurnUsagePill precedent: ProviderDetail.vue wires these named exports).
// Covers: the edit-key submit payload shape, the env-ref branch's
// config patch (env sub-table, never the apiKey field), the
// needs_reconfigure badge view, and the snake_case
// `credential.updated` payload read.

import { describe, expect, it } from 'vitest';

import {
  buildCredentialValue,
  buildEnvRefConfigPatch,
  credentialBadgeFor,
  credentialUpdatedProviderId,
  envApiKeyVarName,
  isEnvRef,
  toProviderCredentialInfo,
} from '../src/lib/providerCredentials';

describe('toProviderCredentialInfo', () => {
  it('should map the snake_case wire row onto the app shape', () => {
    expect(
      toProviderCredentialInfo({
        provider_id: 'openai',
        type: 'key',
        time_created: 1770000000,
        status: 'normal',
      }),
    ).toEqual({ providerId: 'openai', type: 'key', timeCreated: 1770000000, status: 'normal' });
  });

  it('should carry external_available only when the wire row has it', () => {
    const external = toProviderCredentialInfo({
      provider_id: 'azure-lab',
      type: 'external',
      time_created: 1770000000,
      status: 'normal',
      external_available: true,
    });
    expect(external.externalAvailable).toBe(true);

    const key = toProviderCredentialInfo({
      provider_id: 'openai',
      type: 'key',
      time_created: 1770000000,
    });
    expect('externalAvailable' in key).toBe(false);
  });
});

describe('isEnvRef', () => {
  it('should accept ${VAR} and ${env:VAR} spellings only', () => {
    expect(isEnvRef('${MY_KEY}')).toBe(true);
    expect(isEnvRef('${env:MY_KEY}')).toBe(true);
    expect(isEnvRef('sk-literal')).toBe(false);
    expect(isEnvRef('${1BAD}')).toBe(false);
    expect(isEnvRef('${A}${B}')).toBe(false);
  });
});

describe('buildCredentialValue', () => {
  it('should build the key payload {type:"key", key} from the key form', () => {
    expect(buildCredentialValue({ formKind: 'key', key: 'sk-test', resourceName: '' })).toEqual({
      type: 'key',
      key: 'sk-test',
    });
  });

  it('should build the external payload from the azure-cli form', () => {
    expect(
      buildCredentialValue({ formKind: 'azure-cli', key: '', resourceName: 'https://management.core.windows.net/' }),
    ).toEqual({
      type: 'external',
      method_id: 'azure-cli',
      metadata: { resource_name: 'https://management.core.windows.net/' },
    });
  });

  it('should refuse an empty form instead of sending a partial payload', () => {
    expect(buildCredentialValue({ formKind: 'key', key: '   ', resourceName: '' })).toBeNull();
    expect(buildCredentialValue({ formKind: 'azure-cli', key: '', resourceName: ' ' })).toBeNull();
  });
});

describe('buildEnvRefConfigPatch', () => {
  it('should write the conventional env sub-table key for the provider type', () => {
    expect(buildEnvRefConfigPatch({ id: 'my-anthropic', type: 'anthropic' }, '${ANTHROPIC_API_KEY}')).toEqual({
      providers: { 'my-anthropic': { env: { ANTHROPIC_API_KEY: '${ANTHROPIC_API_KEY}' } } },
    });
  });

  it('should never touch the apiKey field in the patch', () => {
    const patch = buildEnvRefConfigPatch({ id: 'my-openai', type: 'openai' }, '${OPENAI_API_KEY}');
    const record = patch?.providers['my-openai'];
    expect(record).toBeDefined();
    expect(Object.keys(record!)).toEqual(['env']);
  });

  it('should return null for a type with no conventional env key', () => {
    expect(buildEnvRefConfigPatch({ id: 'custom-x', type: 'custom' }, '${WHATEVER}')).toBeNull();
  });

  it('should cover every wire type the engine resolves env keys for', () => {
    // The SAME spellings the rust engine reads (config_factory's
    // api_key_env_name): anthropic / openai / openai_responses / google
    // family / kimi.
    expect(envApiKeyVarName('anthropic')).toBe('ANTHROPIC_API_KEY');
    expect(envApiKeyVarName('openai')).toBe('OPENAI_API_KEY');
    expect(envApiKeyVarName('openai_responses')).toBe('OPENAI_API_KEY');
    expect(envApiKeyVarName('google')).toBe('GOOGLE_API_KEY');
    expect(envApiKeyVarName('google-genai')).toBe('GOOGLE_API_KEY');
    expect(envApiKeyVarName('google_genai')).toBe('GOOGLE_API_KEY');
    expect(envApiKeyVarName('kimi')).toBe('MIRRI_API_KEY');
    expect(envApiKeyVarName('vertexai')).toBeUndefined();
  });
});

describe('credentialBadgeFor', () => {
  it('should show the needs_reconfigure badge with top precedence when the store entry is flagged', () => {
    const badge = credentialBadgeFor(
      { providerId: 'openai', type: 'key', timeCreated: 1, status: 'needs_reconfigure' },
      { apiKeyDisplay: null },
    );
    expect(badge).toEqual({ variant: 'danger', labelKey: 'keyNeedsReconfigure' });
  });

  it('should show the external badge for an external entry', () => {
    const badge = credentialBadgeFor(
      { providerId: 'lab', type: 'external', timeCreated: 1, status: 'normal' },
      { apiKeyDisplay: null },
    );
    expect(badge).toEqual({ variant: 'info', labelKey: 'keyExternal' });
  });

  it('should show the plain key-set badge for a healthy key entry', () => {
    const badge = credentialBadgeFor(
      { providerId: 'openai', type: 'key', timeCreated: 1, status: 'normal' },
      { apiKeyDisplay: null },
    );
    expect(badge).toEqual({ variant: 'success', labelKey: 'keySet' });
  });

  it('should show the env-ref literal when no store entry exists', () => {
    const badge = credentialBadgeFor(undefined, { apiKeyDisplay: '${MY_KEY}' });
    expect(badge).toEqual({ variant: 'info', labelKey: 'keyEnvRef', envRef: '${MY_KEY}' });
  });

  it('should show the env-tier success badge when only hasApiKey is set', () => {
    // The rust backend never fills apiKeyDisplay: a provider whose
    // credentials resolve only through the env chain answers hasApiKey
    // with no store entry and no env-ref display — the badge must not
    // fall through to "not set" (the credential is live).
    expect(credentialBadgeFor(undefined, { apiKeyDisplay: null, hasApiKey: true })).toEqual({
      variant: 'success',
      labelKey: 'keyEnvTier',
    });
  });

  it('should show not-set when neither a store entry nor an env ref exists', () => {
    expect(credentialBadgeFor(undefined, { apiKeyDisplay: null })).toEqual({
      variant: 'neutral',
      labelKey: 'keyNotSet',
    });
  });
});

describe('credentialUpdatedProviderId', () => {
  it('should read the snake_case provider_id off the notification payload', () => {
    expect(credentialUpdatedProviderId({ provider_id: 'openai' })).toBe('openai');
  });

  it('should return undefined for malformed payloads', () => {
    expect(credentialUpdatedProviderId(null)).toBeUndefined();
    expect(credentialUpdatedProviderId('openai')).toBeUndefined();
    expect(credentialUpdatedProviderId({})).toBeUndefined();
    expect(credentialUpdatedProviderId({ provider_id: 42 })).toBeUndefined();
  });
});
