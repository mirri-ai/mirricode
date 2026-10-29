// apps/mirri-web/src/lib/providerCredentials.ts
// Provider-credential presentation logic for the settings page (Task 4/5 of
// the credential closeout): the credential-store face is data-driven from
// `credential.list`, and the edit dialog builds its submit payload here. The
// repo has no @vue/test-utils, so these are pure named exports tested
// directly (the TurnUsagePill.vue precedent) — ProviderDetail.vue wires
// them into its template.

import type { AppProvider, ProviderCredentialInfo } from '../api/types';

export type { ProviderCredentialInfo };

/** Map one snake_case `credential.list` wire row onto the app shape
 *  (the standard mapper pattern — tolerant of absent optional fields). */
export function toProviderCredentialInfo(row: {
  provider_id: string;
  type: string;
  time_created: number;
  status?: string;
  external_available?: boolean;
}): ProviderCredentialInfo {
  return {
    providerId: row.provider_id,
    type: row.type,
    timeCreated: row.time_created,
    status: row.status ?? 'normal',
    ...(row.external_available !== undefined ? { externalAvailable: row.external_available } : {}),
  };
}

/** The conventional env-var key each provider type reads from its
 *  `[providers.<id>.env]` sub-table — the SAME spelling table the rust
 *  engine resolves with (`api_key_env_name` in config_factory.rs); a
 *  type with no convention has no env sub-table row to write. */
export function envApiKeyVarName(providerType: string): string | undefined {
  switch (providerType) {
    case 'anthropic':
      return 'ANTHROPIC_API_KEY';
    case 'openai':
    case 'openai_responses':
      return 'OPENAI_API_KEY';
    case 'google':
    case 'google-genai':
    case 'google_genai':
      return 'GOOGLE_API_KEY';
    case 'kimi':
      return 'MIRRI_API_KEY';
    default:
      return undefined;
  }
}

/** Whether a value typed into the edit-key dialog is a pure env-var
 *  reference (`${MY_VAR}` / `${env:MY_VAR}`) — the config-side tier, not
 *  the credential store. */
export function isEnvRef(value: string): boolean {
  return /^\$\{(?:env:)?[A-Za-z_][A-Za-z0-9_]*\}$/.test(value.trim());
}

/** The type of credential the edit dialog collects (Task 5): a literal
 *  key, or an external-source registration (Azure CLI). */
export type CredentialFormKind = 'key' | 'azure-cli';

/** The edit dialog's submit payload. */
export interface CredentialSubmitInput {
  formKind: CredentialFormKind;
  /** The literal key (formKind 'key'). */
  key: string;
  /** The Azure resource name (formKind 'azure-cli'). */
  resourceName: string;
}

/** Build the `credential.set` value payload for the edit dialog's input:
 *  a key form answers `{type:'key', key}`, an azure-cli form answers
 *  `{type:'external', method_id:'azure-cli', metadata:{resource_name}}`.
 *  Returns null when the form is incomplete (the caller shows its own
 *  field error — no partial payloads ever reach the wire). */
export function buildCredentialValue(input: CredentialSubmitInput):
  | { type: 'key'; key: string }
  | { type: 'external'; method_id: string; metadata: { resource_name: string } }
  | null {
  const key = input.key.trim();
  const resourceName = input.resourceName.trim();
  if (input.formKind === 'key') {
    return key ? { type: 'key', key } : null;
  }
  return resourceName
    ? { type: 'external', method_id: 'azure-cli', metadata: { resource_name: resourceName } }
    : null;
}

/** The env sub-table patch the env-ref branch writes through `setConfig`:
 *  `providers.<id>.env.<CONVENTIONAL_KEY>` — the apiKey field is never
 *  touched (env references are config-side facts, not credentials).
 *  Returns null when the type has no conventional key (the caller shows
 *  an unsupported-type error instead of guessing). */
export function buildEnvRefConfigPatch(
  provider: Pick<AppProvider, 'id' | 'type'>,
  envRef: string,
): { providers: Record<string, { env: Record<string, string> }> } | null {
  const varName = envApiKeyVarName(provider.type);
  if (varName === undefined) return null;
  return {
    providers: {
      [provider.id]: { env: { [varName]: envRef.trim() } },
    },
  };
}

/** The credential badge the detail header shows for a provider: the
 *  presence/status view over the store row + the legacy env-ref display. */
export type ProviderCredentialBadge = {
  variant: 'success' | 'info' | 'neutral' | 'danger';
  /** i18n key under settings.models for the badge label. */
  labelKey: string;
  /** The env-ref literal to render inside the badge (env-ref form only). */
  envRef?: string;
};

/** Resolve the credential badge for a provider from its store row (may be
 *  absent — no entry stored) and its env-ref display. Precedence: a
 *  needs_reconfigure store entry outranks everything (the auth-failure
 *  signal must be unmissable); then a stored entry (type-aware label);
 *  then the env-ref tier; then the env-chain tier (hasApiKey with no
 *  store entry and no display — the rust backend never fills
 *  apiKeyDisplay, its env-chain resolution is invisible on the wire);
 *  then "not set". */
export function credentialBadgeFor(
  credential: ProviderCredentialInfo | undefined,
  provider: Pick<AppProvider, 'apiKeyDisplay' | 'hasApiKey'>,
): ProviderCredentialBadge {
  if (credential !== undefined) {
    if (credential.status === 'needs_reconfigure') {
      return { variant: 'danger', labelKey: 'keyNeedsReconfigure' };
    }
    if (credential.type === 'external') {
      return { variant: 'info', labelKey: 'keyExternal' };
    }
    return { variant: 'success', labelKey: 'keySet' };
  }
  if (provider.apiKeyDisplay) {
    return { variant: 'info', labelKey: 'keyEnvRef', envRef: provider.apiKeyDisplay };
  }
  if (provider.hasApiKey) {
    return { variant: 'success', labelKey: 'keyEnvTier' };
  }
  return { variant: 'neutral', labelKey: 'keyNotSet' };
}

/** Read the snake_case `provider_id` off a `credential.updated`
 *  notification payload (`{"provider_id": "..."}`). */
export function credentialUpdatedProviderId(payload: unknown): string | undefined {
  if (typeof payload !== 'object' || payload === null) return undefined;
  const id = (payload as Record<string, unknown>)['provider_id'];
  return typeof id === 'string' ? id : undefined;
}
