/**
 * B4-COEX Part A1: Shared `workspaces.json` cross-engine read/write.
 *
 * Verifies that v1's `workspaceRegistryFile.ts` and v2's
 * `FileWorkspacePersistence` agree on the on-disk format of
 * `<homeDir>/workspaces.json`:
 *
 * - v1 writes → v2 reads the same catalog
 * - v2 writes → v1 reads the same catalog
 * - Tombstones (`deleted_workspace_ids`) round-trip across engines
 * - Both engines use the same `version` field
 * - ISO date strings round-trip losslessly
 *
 * Since agent-core-v2 cannot directly import from agent-core (different
 * package, no dependency), this test replicates v1's read/write logic
 * (which is a small, stable, well-specified pure function) and verifies
 * bidirectional format compatibility against v2's real production code.
 *
 * The v1 format (from `workspaceRegistryFile.ts`):
 * ```
 * interface WorkspaceRegistryFile {
 *   version: number;                    // always 1
 *   workspaces: Record<string, {
 *     root: string;
 *     name: string;
 *     created_at: string;              // ISO 8601
 *     last_opened_at: string;          // ISO 8601
 *   }>;
 *   deleted_workspace_ids: string[];
 * }
 * ```
 *
 * The v2 format (from `FileWorkspacePersistence` + `workspacePersistence.ts`):
 * Same shape. `load()` converts `created_at`/`last_opened_at` strings to
 * epoch ms; `save()` converts back to ISO strings. The on-disk JSON is
 * identical.
 *
 * Run: `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run
 *      test/coexistence/shared-workspaces-json.test.ts`.
 */

import { mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { mkdtemp } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, describe, expect, it } from 'vitest';

// v2 code — real production classes
import { FileWorkspacePersistence } from '#/app/workspace/fileWorkspacePersistence';
import { JsonAtomicDocumentStore } from '#/persistence/backends/node-fs/atomicDocumentStore';
import { FileStorageService } from '#/persistence/backends/node-fs/fileStorageService';
import type { WorkspaceCatalog } from '#/app/workspace/workspacePersistence';

// ---------------------------------------------------------------------------
// v1-compatible read/write (replicated from workspaceRegistryFile.ts)
// ---------------------------------------------------------------------------

interface V1WorkspaceRegistryEntry {
  root: string;
  name: string;
  created_at: string;
  last_opened_at: string;
}

interface V1WorkspaceRegistryFile {
  version: number;
  workspaces: Record<string, V1WorkspaceRegistryEntry>;
  deleted_workspace_ids: string[];
}

async function v1ReadWorkspaceRegistryFile(homeDir: string): Promise<V1WorkspaceRegistryFile> {
  const registryPath = join(homeDir, 'workspaces.json');
  let raw: string;
  try {
    raw = await readFile(registryPath, 'utf8');
  } catch (error) {
    const code = (error as NodeJS.ErrnoException).code;
    if (code === 'ENOENT' || code === 'ENOTDIR') {
      return { version: 1, workspaces: {}, deleted_workspace_ids: [] };
    }
    throw error;
  }
  try {
    const parsed = JSON.parse(raw);
    if (typeof parsed !== 'object' || parsed === null || typeof parsed.workspaces !== 'object') {
      return { version: 1, workspaces: {}, deleted_workspace_ids: [] };
    }
    return parsed as V1WorkspaceRegistryFile;
  } catch {
    return { version: 1, workspaces: {}, deleted_workspace_ids: [] };
  }
}

async function v1WriteWorkspaceRegistryFile(
  homeDir: string,
  file: V1WorkspaceRegistryFile,
): Promise<void> {
  const registryPath = join(homeDir, 'workspaces.json');
  await mkdir(homeDir, { recursive: true });
  const tmp = `${registryPath}.tmp`;
  await writeFile(tmp, JSON.stringify(file, null, 2), 'utf8');
  const { rename } = await import('node:fs/promises');
  await rename(tmp, registryPath);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const cleanups: string[] = [];

async function makeTempHome(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), 'coex-workspace-'));
  cleanups.push(dir);
  return dir;
}

afterEach(async () => {
  for (const dir of cleanups.splice(0)) {
    await rm(dir, { recursive: true, force: true }).catch(() => {});
  }
});

/**
 * Build a v2 `FileWorkspacePersistence` pointed at `homeDir`.
 * Chains: FileWorkspacePersistence → JsonAtomicDocumentStore → FileStorageService.
 */
function makeV2Persistence(homeDir: string): FileWorkspacePersistence {
  const storage = new FileStorageService(homeDir);
  const atomicStore = new JsonAtomicDocumentStore(storage);
  return new FileWorkspacePersistence(atomicStore);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe('B4-COEX: shared workspaces.json cross-engine read/write', () => {
  it('should read a catalog written by v1 when v2 loads it', async () => {
    const homeDir = await makeTempHome();
    const now = new Date().toISOString();

    // v1 writes a catalog
    const v1File: V1WorkspaceRegistryFile = {
      version: 1,
      workspaces: {
        ws1: { root: '/tmp/project-a', name: 'project-a', created_at: now, last_opened_at: now },
        ws2: { root: '/tmp/project-b', name: 'project-b', created_at: now, last_opened_at: now },
      },
      deleted_workspace_ids: ['ws-deleted'],
    };
    await v1WriteWorkspaceRegistryFile(homeDir, v1File);

    // v2 reads the same file
    const v2Persistence = makeV2Persistence(homeDir);
    const catalog = await v2Persistence.load();

    expect(catalog).toBeDefined();
    expect(catalog!.workspaces).toHaveLength(2);
    expect(catalog!.workspaces[0]).toMatchObject({
      root: '/tmp/project-a',
      name: 'project-a',
    });
    expect(catalog!.workspaces[1]).toMatchObject({
      root: '/tmp/project-b',
      name: 'project-b',
    });
    expect(catalog!.deletedIds).toContain('ws-deleted');
  });

  it('should read a catalog written by v2 when v1 loads it', async () => {
    const homeDir = await makeTempHome();
    const now = Date.now();

    // v2 writes a catalog
    const v2Persistence = makeV2Persistence(homeDir);
    const catalog: WorkspaceCatalog = {
      workspaces: [
        { id: 'ws3', root: '/tmp/project-c', name: 'project-c', createdAt: now, lastOpenedAt: now },
        { id: 'ws4', root: '/tmp/project-d', name: 'project-d', createdAt: now, lastOpenedAt: now },
      ],
      deletedIds: ['ws-gone'],
    };
    await v2Persistence.save(catalog);

    // v1 reads the same file
    const v1File = await v1ReadWorkspaceRegistryFile(homeDir);

    expect(Object.keys(v1File.workspaces)).toHaveLength(2);
    expect(v1File.workspaces['ws3']).toMatchObject({
      root: '/tmp/project-c',
      name: 'project-c',
    });
    expect(v1File.workspaces['ws4']).toMatchObject({
      root: '/tmp/project-d',
      name: 'project-d',
    });
    expect(v1File.deleted_workspace_ids).toContain('ws-gone');
    expect(v1File.version).toBe(1);
  });

  it('should preserve tombstones across a v1-write → v2-read → v2-write → v1-read cycle', async () => {
    const homeDir = await makeTempHome();
    const now = new Date().toISOString();

    // v1 writes with a tombstone
    await v1WriteWorkspaceRegistryFile(homeDir, {
      version: 1,
      workspaces: {
        ws1: { root: '/tmp/project-a', name: 'project-a', created_at: now, last_opened_at: now },
      },
      deleted_workspace_ids: ['ws-old'],
    });

    // v2 reads and writes back (possibly adding a workspace)
    const v2Persistence = makeV2Persistence(homeDir);
    let catalog = await v2Persistence.load();
    expect(catalog!.deletedIds).toContain('ws-old');

    // v2 adds a workspace
    catalog = {
      workspaces: [
        ...catalog!.workspaces,
        { id: 'ws2', root: '/tmp/project-b', name: 'project-b', createdAt: Date.now(), lastOpenedAt: Date.now() },
      ],
      deletedIds: catalog!.deletedIds,
    };
    await v2Persistence.save(catalog);

    // v1 reads back — both workspaces present, tombstone survived
    const v1File = await v1ReadWorkspaceRegistryFile(homeDir);
    expect(Object.keys(v1File.workspaces)).toHaveLength(2);
    expect(v1File.deleted_workspace_ids).toContain('ws-old');
  });

  it('should agree on the version field (both use version 1)', async () => {
    const homeDir = await makeTempHome();
    const now = new Date().toISOString();

    // v1 writes
    await v1WriteWorkspaceRegistryFile(homeDir, {
      version: 1,
      workspaces: {
        ws1: { root: '/tmp/project-a', name: 'project-a', created_at: now, last_opened_at: now },
      },
      deleted_workspace_ids: [],
    });

    // Read via v1
    const v1File = await v1ReadWorkspaceRegistryFile(homeDir);
    expect(v1File.version).toBe(1);

    // Read via v2, then v2 writes back
    const v2Persistence = makeV2Persistence(homeDir);
    const catalog = await v2Persistence.load();
    await v2Persistence.save(catalog!);

    // v1 re-reads — version still 1
    const v1File2 = await v1ReadWorkspaceRegistryFile(homeDir);
    expect(v1File2.version).toBe(1);
  });

  it('should tolerate missing file (both return empty catalog)', async () => {
    const homeDir = await makeTempHome();

    const v1File = await v1ReadWorkspaceRegistryFile(homeDir);
    expect(v1File.workspaces).toEqual({});

    const v2Persistence = makeV2Persistence(homeDir);
    const catalog = await v2Persistence.load();
    expect(catalog).toBeUndefined();
  });

  it('should preserve ISO date strings across v2-save → v1-read round-trip', async () => {
    const homeDir = await makeTempHome();
    const nowMs = Date.now();

    // v2 writes with epoch ms
    const v2Persistence = makeV2Persistence(homeDir);
    await v2Persistence.save({
      workspaces: [
        { id: 'ws1', root: '/tmp/project-a', name: 'project-a', createdAt: nowMs, lastOpenedAt: nowMs },
      ],
      deletedIds: [],
    });

    // v1 reads — the dates should be ISO 8601 strings
    const v1File = await v1ReadWorkspaceRegistryFile(homeDir);
    const entry = v1File.workspaces['ws1'];
    expect(entry).toBeDefined();
    expect(typeof entry!.created_at).toBe('string');
    expect(typeof entry!.last_opened_at).toBe('string');
    // Verify the round-trip is lossless: v1 reads the same epoch
    expect(new Date(entry!.created_at).getTime()).toBe(nowMs);
    expect(new Date(entry!.last_opened_at).getTime()).toBe(nowMs);
  });
});
