/**
 * B4-COEX Part A3: `/meta.backend` discriminator.
 *
 * Verifies the coexistence contract that v1 omits `backend` from its meta
 * response while v2 returns `backend: 'v2'`. The web UI keys its backend
 * switcher pill off this field.
 *
 * This test reads the source files directly and verifies the code structure.
 * Paths are derived from `process.cwd()` which, under vitest, is the
 * `packages/agent-core-v2` directory.
 *
 * Run: `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run
 *      test/coexistence/backend-switch.test.ts`.
 */

import { readFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { describe, expect, it } from 'vitest';

// __dirname = packages/agent-core-v2/test/coexistence — resolve relative to the
// test file itself (not process.cwd()) so the paths work under both
// `pnpm --filter ... exec vitest run` and root-level `vitest run`.
const PKGS = join(import.meta.dirname, '..', '..', '..');

const V2_META_ROUTE = join(PKGS, 'kap-server', 'src', 'routes', 'meta.ts');
const V1_META_ROUTE = join(PKGS, 'server', 'src', 'routes', 'meta.ts');
const V2_META_SCHEMA = join(PKGS, 'kap-server', 'src', 'protocol', 'rest-meta.ts');

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe('B4-COEX: /meta.backend discriminator', () => {
  it('should confirm v2 meta route includes backend:"v2" in response data', async () => {
    const v2Source = await readFile(V2_META_ROUTE, 'utf8');

    // The v2 route freezes data with `backend: 'v2'`
    expect(v2Source).toMatch(/backend:\s*['"]v2['"]/);
  });

  it('should confirm v1 meta route does NOT include backend field', async () => {
    const v1Source = await readFile(V1_META_ROUTE, 'utf8');

    // v1 has no `backend` field in the data object
    expect(v1Source).not.toMatch(/backend:/);
  });

  it('should confirm v2 protocol schema types backend as optional enum', async () => {
    const schemaSource = await readFile(V2_META_SCHEMA, 'utf8');

    // The schema should define backend as optional
    expect(schemaSource).toMatch(/backend.*optional/);
    // The schema should define backend as an enum of v1/v2
    expect(schemaSource).toMatch(/z\.enum.*v1.*v2/);
  });

  it('should confirm both servers use compatible MetaResponse shapes', async () => {
    const [v2Source, v1Source] = await Promise.all([
      readFile(V2_META_ROUTE, 'utf8'),
      readFile(V1_META_ROUTE, 'utf8'),
    ]);

    // Both have server_version
    expect(v2Source).toContain('server_version');
    expect(v1Source).toContain('server_version');

    // Both have capabilities
    expect(v2Source).toContain('capabilities');
    expect(v1Source).toContain('capabilities');

    // Both have server_id
    expect(v2Source).toContain('server_id');
    expect(v1Source).toContain('server_id');

    // Both have started_at
    expect(v2Source).toContain('started_at');
    expect(v1Source).toContain('started_at');

    // Only v2 has backend
    expect(v2Source).toMatch(/backend:/);
    expect(v1Source).not.toMatch(/backend:/);
  });
});
