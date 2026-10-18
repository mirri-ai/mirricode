import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { hasCachedOAuthToken } from '../../src/services/auth/cachedAccessToken';

let tmpHome: string;

beforeEach(() => {
  tmpHome = mkdtempSync(join(tmpdir(), 'mirri-cached-token-'));
});

afterEach(() => {
  rmSync(tmpHome, { recursive: true, force: true });
});

function seedToken(stem: string, body: unknown = { access_token: 'tok' }): void {
  mkdirSync(join(tmpHome, 'credentials'), { recursive: true });
  writeFileSync(join(tmpHome, 'credentials', `${stem}.json`), JSON.stringify(body), 'utf8');
}

describe('hasCachedOAuthToken', () => {
  it('given a default oauth key and a token file at credentials/mirri-code.json, when the probe runs, then it reports cached', async () => {
    seedToken('mirri-code');

    await expect(hasCachedOAuthToken(tmpHome, 'oauth/mirri-code')).resolves.toBe(true);
  });

  it('given a token file named after the provider id instead of the key stem, when the probe runs, then it reports uncached', async () => {
    // The managed provider id must never be used as the storage name.
    seedToken('managed:mirri-code');

    await expect(hasCachedOAuthToken(tmpHome, 'oauth/mirri-code')).resolves.toBe(false);
    await expect(hasCachedOAuthToken(tmpHome, 'mirri-code')).resolves.toBe(false);
  });

  it('given a custom oauth key and a token file at the derived stem, when the probe runs, then it reports cached', async () => {
    seedToken('custom-mirri-code');

    await expect(
      hasCachedOAuthToken(tmpHome, 'oauth/custom-mirri-code'),
    ).resolves.toBe(true);
  });

  it('given a bare oauth key, when the probe runs, then it reads credentials/<key>.json', async () => {
    seedToken('custom-key');

    await expect(hasCachedOAuthToken(tmpHome, 'custom-key')).resolves.toBe(true);
  });

  it('given no oauth key, when the probe runs, then it falls back to the default mirri-code stem', async () => {
    seedToken('mirri-code');

    await expect(hasCachedOAuthToken(tmpHome)).resolves.toBe(true);
  });

  it('given a token file without an access token, when the probe runs, then it reports uncached', async () => {
    seedToken('mirri-code', { refresh_token: 'abc' });

    await expect(hasCachedOAuthToken(tmpHome, 'oauth/mirri-code')).resolves.toBe(false);
  });

  it('given a corrupt token file, when the probe runs, then it reports uncached', async () => {
    seedToken('mirri-code', '{ not json');

    await expect(hasCachedOAuthToken(tmpHome, 'oauth/mirri-code')).resolves.toBe(false);
  });

  it('given an invalid oauth key, when the probe runs, then it reports uncached without throwing', async () => {
    await expect(hasCachedOAuthToken(tmpHome, 'oauth/')).resolves.toBe(false);
    await expect(hasCachedOAuthToken(tmpHome, '.hidden')).resolves.toBe(false);
  });
});