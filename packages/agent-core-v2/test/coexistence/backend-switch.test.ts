/**
 * B4-COEX Part A3: `/meta.backend` discriminator.
 *
 * Verifies the v2 meta contract: kap-server's `/meta` response carries
 * `backend: 'v2'` for the web UI's backend switcher pill, and the protocol
 * schema types `backend` as an optional v1/v2 enum.
 *
 * The v1 server (which omitted `backend`) was retired with the v1→v2
 * migration — the original coexistence comparison against
 * `packages/server/src/routes/meta.ts` was removed when that package was
 * deleted.
 *
 * This test reads the source files directly and verifies the code structure.
 * Paths are derived from `process.cwd()` which, under vitest, is the
 * `packages/agent-core-v2` directory.
 *
 * Run: `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run
 *      test/coexistence/backend-switch.test.ts`.
 */

import { readFile } from 'node:fs/promises';
import { join } from 'node:path';

import { describe, expect, it } from 'vitest';

// __dirname = packages/agent-core-v2/test/coexistence — resolve relative to the
// test file itself (not process.cwd()) so the paths work under both
// `pnpm --filter ... exec vitest run` and root-level `vitest run`.
const PKGS = join(import.meta.dirname, '..', '..', '..');

const V2_META_ROUTE = join(PKGS, 'kap-server', 'src', 'routes', 'meta.ts');
const V2_META_SCHEMA = join(PKGS, 'kap-server', 'src', 'protocol', 'rest-meta.ts');

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe('meta backend discriminator (v2 server)', () => {
  it('should confirm the v2 meta route includes backend:"v2" in response data', async () => {
    const v2Source = await readFile(V2_META_ROUTE, 'utf8');

    // The v2 route freezes data with `backend: 'v2'`
    expect(v2Source).toMatch(/backend:\s*['"]v2['"]/);
  });

  it('should confirm the v2 meta response shape exposes the full capability map', async () => {
    const v2Source = await readFile(V2_META_ROUTE, 'utf8');

    // Every capability the web UI's switcher relies on is declared
    expect(v2Source).toContain('server_version');
    expect(v2Source).toContain('capabilities');
    expect(v2Source).toContain('server_id');
    expect(v2Source).toContain('started_at');
    expect(v2Source).toContain('backend:');
  });

  it('should confirm the v2 protocol schema types backend as an optional enum', async () => {
    const schemaSource = await readFile(V2_META_SCHEMA, 'utf8');

    // The schema should define backend as optional
    expect(schemaSource).toMatch(/backend.*optional/);
    // The schema should define backend as an enum of v1/v2
    expect(schemaSource).toMatch(/z\.enum.*v1.*v2/);
  });
});