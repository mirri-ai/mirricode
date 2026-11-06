// apps/mirri-web/src/lib/providerBaseUrl.ts
// Base-URL edit logic for the provider detail pane (#187): the pure
// payload/decision helpers ProviderDetail.vue wires into its edit dialog.
// The FIRST_PARTY set mirrors the rust engine's first-party defaults (the
// providers whose implicit base URL is sanctioned, #184): openai /
// anthropic / google-genai (plus their config spellings).

import type { AppProvider } from '../api/types';

/** The provider ids whose implicit official base URL is sanctioned — the
 *  same first-party set the engine keeps defaults for (provider id ==
 *  first-party type). */
const FIRST_PARTY_PROVIDER_IDS = new Set([
  'openai',
  'anthropic',
  'google-genai',
  'google',
  'google_genai',
  'kimi',
]);

/** Build the `setConfig` patch for a base-URL edit: a trimmed value writes
 *  `providers.<id>.baseUrl`; an empty/whitespace-only value writes an
 *  EMPTY STRING (the engine treats it as unset — the klient `null` is
 *  stripped by `config.set`'s not-provided semantics, so "" is the clear
 *  encoding). */
export function buildBaseUrlConfigPatch(
  provider: Pick<AppProvider, 'id'>,
  baseUrl: string,
): { providers: Record<string, { baseUrl: string }> } {
  const trimmed = baseUrl.trim();
  return {
    providers: {
      [provider.id]: { baseUrl: trimmed },
    },
  };
}

/** Whether clearing the base URL needs a confirmation dialog: a
 *  first-party provider (openai / anthropic / google-genai) silently
 *  restores its official default, while any other provider left without a
 *  base URL will refuse to build — the user must confirm that drop. */
export function clearingBaseUrlNeedsConfirm(
  provider: Pick<AppProvider, 'id'>,
  nextValue: string,
): boolean {
  if (nextValue.trim() !== '') return false;
  return !FIRST_PARTY_PROVIDER_IDS.has(provider.id);
}
