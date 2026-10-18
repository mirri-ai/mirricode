/**
 * Minimal cached-OAuth-token existence probe for config-level `oauth`
 * providers (e.g. an anthropic provider with `oauth = { ... }` in
 * config.toml).
 *
 * After the managed-login retirement, agent-core no longer owns OAuth
 * runtime (device-code login, live refresh) — that lives in the SDK host's
 * auth facade. What remains here is a read-only existence check against the
 * same on-disk layout the host toolkit persists:
 * `<homeDir>/credentials/<stem>.json` carrying a wire-format token with a
 * non-empty `access_token` field.
 *
 * The filename stem is derived from the provider's **oauth key**, never from
 * the provider id — the toolkit persists tokens keyed by the oauth key
 * (`resolveMirriTokenStorageName` in the retired oauth toolkit), so a
 * `managed:mirri-code` provider with `oauth.key = "oauth/mirri-code"` stores
 * its token at `credentials/mirri-code.json`, not at
 * `credentials/managed:mirri-code.json`.
 *
 * Semantics mirror the old toolkit probe: a missing / corrupt / empty
 * file counts as "no cached token", never throws.
 */

import { readFile } from 'node:fs/promises';
import { join } from 'node:path';

/** Default `oauth.key` used when a provider's oauth ref declares none. */
const DEFAULT_OAUTH_KEY = 'oauth/mirri-code';

/**
 * Mirrors the toolkit's `resolveMirriTokenStorageName` rule exactly:
 *   - `mirri-code` / `oauth/mirri-code` (the default key) → `mirri-code`
 *   - `oauth/<x>` (non-empty `x`) → `<x>`
 *   - a bare key with no `/` and no leading `.` → the key itself
 *   - anything else → throws `Invalid Mirri OAuth token key: "<key>"`.
 */
function cachedTokenStorageName(oauthKey: string | undefined): string {
  const key = oauthKey ?? DEFAULT_OAUTH_KEY;
  if (key === 'mirri-code' || key === DEFAULT_OAUTH_KEY) return 'mirri-code';

  const prefix = 'oauth/';
  if (key.startsWith(prefix) && key.slice(prefix.length).length > 0) {
    return key.slice(prefix.length);
  }

  if (!key.includes('/') && !key.startsWith('.')) return key;
  throw new Error(`Invalid Mirri OAuth token key: "${key}".`);
}

/**
 * True when a non-empty access token is persisted for the provider identified
 * by `oauthKey` (default `oauth/mirri-code`) under `homeDir`'s credentials
 * directory. The key (not the provider id) determines the file stem. Never
 * throws.
 */
export async function hasCachedOAuthToken(
  homeDir: string,
  oauthKey?: string | undefined,
): Promise<boolean> {
  let stem: string;
  try {
    stem = cachedTokenStorageName(oauthKey);
  } catch {
    // Invalid key — mirrors the old caller behavior of treating any probe
    // failure as "no cached token".
    return false;
  }

  let raw: string;
  try {
    raw = await readFile(join(homeDir, 'credentials', `${stem}.json`), 'utf-8');
  } catch {
    return false;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return false;
  }
  if (typeof parsed !== 'object' || parsed === null) return false;
  const accessToken = (parsed as { readonly access_token?: unknown }).access_token;
  return typeof accessToken === 'string' && accessToken.trim().length > 0;
}