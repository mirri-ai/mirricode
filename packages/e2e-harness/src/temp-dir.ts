/**
 * Temp directory helpers for e2e tests (moved out of the retired
 * engine-fixture module — the fake-provider tests still use them).
 */
import { rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

/** Create a unique temp directory (cleanup is the caller's job). */
export async function makeTempDir(): Promise<string> {
  const { mkdtemp: mk } = await import('node:fs/promises');
  return mk(join(tmpdir(), 'mirri-e2e-'));
}

/** Remove a temp directory recursively, ignoring "already gone" errors. */
export async function removeTempDir(dir: string): Promise<void> {
  await rm(dir, { recursive: true, force: true });
}
