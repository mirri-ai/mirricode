// apps/mirri-web/src/lib/providerConnection.ts
// Provider connection settings: the pure payload/validation helpers the
// provider edit dialog wires in. Custom API servers and proxies need to
// correct these values after the fact; rebuilding the provider would take the
// model list down with it, so every field here is editable in place.
//
// "Custom" is a creation MODE (fill in id/type/endpoint by hand), never a
// protocol type — the engine has no `custom` protocol and rejects it.

import type { AppConfigProvider } from '../api/types';

/** The protocol types this engine can actually build an adapter for. A type
 *  outside this list would only fail when the first request is attempted, so
 *  it must not be offered (R5). */
export const SUPPORTED_PROVIDER_TYPES = [
  'openai',
  'openai_responses',
  'anthropic',
  'google-genai',
  'kimi',
] as const;

/** The FIRST-PARTY provider ids whose official endpoint is sanctioned, so an
 *  empty base URL falls back to it instead of refusing to build. Keyed on the
 *  provider ID, not the type: the engine's `first_party_default_base_url`
 *  answers only when the id itself is first-party, so an id like `gw` with
 *  type `openai` gets no default and would fail on its first request. */
const FIRST_PARTY_PROVIDER_IDS = new Set([
  'openai',
  'anthropic',
  'google-genai',
  'google',
  'google_genai',
  'kimi',
]);

/** One editable `customHeaders` row. */
export interface HeaderPair {
  name: string;
  value: string;
}

/** Read a header map into ordered rows (sorted by name for a stable form). */
export function toHeaderPairs(headers: Record<string, string> | undefined): HeaderPair[] {
  if (headers === undefined) return [];
  return Object.entries(headers)
    .map(([name, value]) => ({ name, value }))
    .sort((a, b) => a.name.localeCompare(b.name));
}

/** Write rows back into a header map; blank names are dropped and values
 *  trimmed. Answers `undefined` when nothing survives. */
export function fromHeaderPairs(pairs: HeaderPair[]): Record<string, string> | undefined {
  const out: Record<string, string> = {};
  for (const pair of pairs) {
    const name = pair.name.trim();
    if (name === '') continue;
    out[name] = pair.value.trim();
  }
  return Object.keys(out).length > 0 ? out : undefined;
}

/** Validate the connection form. Answers an i18n message key, or `null`. */
export function validateConnection(input: {
  providerId: string;
  type: string;
  baseUrl: string;
  headers: HeaderPair[];
}): string | null {
  const trimmed = input.baseUrl.trim();
  if (trimmed !== '') {
    if (!/^https?:\/\//i.test(trimmed)) {
      return 'settings.models.connection.errorBaseUrlScheme';
    }
  } else if (!FIRST_PARTY_PROVIDER_IDS.has(input.providerId)) {
    return 'settings.models.connection.errorBaseUrlRequired';
  }

  const seen = new Set<string>();
  for (const pair of input.headers) {
    const name = pair.name.trim();
    if (name === '') continue;
    const key = name.toLowerCase();
    if (seen.has(key)) {
      return 'settings.models.connection.errorDuplicateHeader';
    }
    seen.add(key);
  }
  return null;
}

/**
 * Build the single `config.set` patch for a connection edit.
 *
 * Optional fields are written as empty values (not omitted) so the engine's
 * deep merge actually clears them — an omitted key leaves the old value in
 * place. `customHeaders` is the exception that needs replace semantics: the
 * engine treats that one map as replace-on-patch so a deleted header really
 * disappears.
 */
export function buildConnectionPatch(
  providerId: string,
  input: { type: string; baseUrl: string; headers: HeaderPair[]; thinkingOff: string },
): { providers: Record<string, Partial<AppConfigProvider>> } {
  return {
    providers: {
      [providerId]: {
        type: input.type,
        baseUrl: input.baseUrl.trim(),
        customHeaders: fromHeaderPairs(input.headers) ?? {},
        thinkingOff: input.thinkingOff.trim(),
      },
    },
  };
}
