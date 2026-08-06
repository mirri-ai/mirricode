/**
 * Scenario: legacy split buckets (casing/slash spelling variants of one root)
 * register as one workspace. The union listing + id-cursor pagination spans
 * every bucket id without repeats.
 *
 * Kept in its OWN file because the split-bucket catalog interacts with the
 * workspace read-path cache (list vs listWithSessionCounts share a TTL window);
 * in this isolated process the assertions are deterministic. The authoritative
 * read model is exercised: the buckets are seeded to disk AFTER boot, so they
 * only become listable once the sidebar refresh endpoint re-converges them.
 */
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { encodeWorkDirKey } from '@mirri-ai/agent-core-v2/_base/utils/workdir-slug';

import { type RunningServer } from '../src/start';
import { startReadyServer } from './helpers/startReadyServer';
import { TEST_HOST_IDENTITY } from './helpers/hostIdentity';
import { authedFetch } from './helpers/auth';

interface Envelope<T> {
  code: number;
  msg: string;
  data: T;
  request_id: string;
}

interface SessionWire {
  id: string;
}

interface PageWire {
  items: SessionWire[];
  has_more: boolean;
}

describe('server-v2 /api/v1/sessions (legacy split buckets)', () => {
  let server: RunningServer | undefined;
  let home: string | undefined;
  let base: string;

  beforeEach(async () => {
    home = await mkdtemp(join(tmpdir(), 'mirri-server-v2-split-'));
    server = await startReadyServer({
      hostIdentity: TEST_HOST_IDENTITY,
      host: '127.0.0.1',
      port: 0,
      homeDir: home,
      logLevel: 'silent',
      debugEndpoints: true,
    });
    base = `http://127.0.0.1:${server.port}`;
  });

  afterEach(async () => {
    if (server !== undefined) {
      await server.close();
      server = undefined;
    }
    if (home !== undefined) {
      await new Promise((resolve) => setTimeout(resolve, 25));
      await rm(home, { recursive: true, force: true, maxRetries: 5, retryDelay: 50 });
      home = undefined;
    }
  });

  async function postJson<T>(path: string, body?: unknown): Promise<{ status: number; body: Envelope<T> }> {
    const headers: Record<string, string> = {};
    if (body !== undefined) headers['content-type'] = 'application/json';
    const res = await authedFetch(server as RunningServer, base, path, {
      method: 'POST',
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    return { status: res.status, body: (await res.json()) as Envelope<T> };
  }

  async function getJson<T>(path: string): Promise<{ status: number; body: Envelope<T> }> {
    const res = await authedFetch(server as RunningServer, base, path);
    return { status: res.status, body: (await res.json()) as Envelope<T> };
  }

  it('lists the union of legacy split buckets for one workspace, in recency order', async () => {
    // Legacy pre-fold data: one physical directory registered under two
    // spelling variants, with sessions bucketed per minted id.
    const typedRoot = 'C:\\Users\\Foo\\Proj';
    const lowerRoot = 'c:\\users\\foo\\proj';
    const typedId = encodeWorkDirKey(typedRoot);
    const lowerId = encodeWorkDirKey(lowerRoot);
    await writeFile(
      join(home as string, 'workspaces.json'),
      JSON.stringify({
        version: 1,
        workspaces: {
          [typedId]: {
            root: typedRoot,
            name: 'proj',
            created_at: '2024-01-01T00:00:00.000Z',
            last_opened_at: '2024-01-01T00:00:00.000Z',
          },
          [lowerId]: {
            root: lowerRoot,
            name: 'proj',
            created_at: '2024-01-01T00:00:00.000Z',
            last_opened_at: '2024-01-01T00:00:00.000Z',
          },
        },
      }),
      'utf8',
    );
    const seedBucket = async (wsId: string, sid: string, updatedAt: number): Promise<void> => {
      const dir = join(home as string, 'sessions', wsId, sid);
      await mkdir(dir, { recursive: true });
      await writeFile(
        join(dir, 'state.json'),
        JSON.stringify({ version: 2, cwd: typedRoot, createdAt: 1, updatedAt }),
        'utf8',
      );
    };
    await seedBucket(typedId, 's-typed', 50);
    await seedBucket(lowerId, 's-lower', 60);

    // The registry merges the two entries; whichever id survives is the
    // representative the client lists by.
    const workspaces = await getJson<{ items: { id: string }[] }>('/api/v1/workspaces');
    const rep = workspaces.body.data.items.find((it) => it.id === typedId || it.id === lowerId)?.id as string;
    expect([typedId, lowerId]).toContain(rep);

    // Authoritative-read model: these on-disk buckets were written AFTER the
    // server booted, so the sqlite index does not list them until an explicit
    // refresh (the sidebar refresh button).
    const refreshedTyped = await postJson<{ refreshed: boolean; inserted?: number; removed?: number }>(`/api/v1/workspaces/${encodeURIComponent(typedId)}/refresh`, undefined);
    const refreshedLower = await postJson<{ refreshed: boolean; inserted?: number; removed?: number }>(`/api/v1/workspaces/${encodeURIComponent(lowerId)}/refresh`, undefined);
    expect(refreshedTyped.body.code).toBe(0);
    expect(refreshedLower.body.code).toBe(0);

    const listed = await getJson<PageWire>(`/api/v1/sessions?workspace_id=${encodeURIComponent(rep)}`);
    expect(listed.body.code).toBe(0);
    expect(listed.body.data.items.map((s) => s.id)).toEqual(['s-lower', 's-typed']);

    // Id-cursor pagination spans the bucket boundary without repeats.
    const page1 = await getJson<PageWire>(`/api/v1/sessions?workspace_id=${encodeURIComponent(rep)}&page_size=1`);
    expect(page1.body.data.items.map((s) => s.id)).toEqual(['s-lower']);
    expect(page1.body.data.has_more).toBe(true);
    const page2 = await getJson<PageWire>(
      `/api/v1/sessions?workspace_id=${encodeURIComponent(rep)}&page_size=1&before_id=s-lower`,
    );
    expect(page2.body.data.items.map((s) => s.id)).toEqual(['s-typed']);
    expect(page2.body.data.has_more).toBe(false);
  });

  it('refreshing one id expands the alias set and converges the sibling bucket', async () => {
    // Only ONE spelling is registered in the catalog this time; the sibling
    // bucket exists on disk + session_index but never got a registry entry.
    const typedRoot = 'C:\\Users\\Foo\\Proj';
    const lowerRoot = 'c:\\users\\foo\\proj';
    const typedId = encodeWorkDirKey(typedRoot);
    const lowerId = encodeWorkDirKey(lowerRoot);
    expect(lowerId).not.toBe(typedId);
    await writeFile(
      join(home as string, 'workspaces.json'),
      JSON.stringify({
        version: 1,
        workspaces: {
          [typedId]: {
            root: typedRoot,
            name: 'proj',
            created_at: '2024-01-01T00:00:00.000Z',
            last_opened_at: '2024-01-01T00:00:00.000Z',
          },
        },
        deleted_workspace_ids: [],
      }),
      'utf8',
    );
    await writeFile(
      join(home as string, 'session_index.jsonl'),
      [
        JSON.stringify({
          sessionId: 's-lower',
          sessionDir: `sessions/${lowerId}/s-lower`,
          workDir: lowerRoot,
        }),
      ].join('\n') + '\n',
      'utf8',
    );
    const seedBucket = async (wsId: string, sid: string, updatedAt: number): Promise<void> => {
      const dir = join(home as string, 'sessions', wsId, sid);
      await mkdir(dir, { recursive: true });
      await writeFile(
        join(dir, 'state.json'),
        JSON.stringify({ version: 2, cwd: typedRoot, createdAt: 1, updatedAt }),
        'utf8',
      );
    };
    await seedBucket(typedId, 's-typed', 50);
    await seedBucket(lowerId, 's-lower', 60);

    // Refresh ONLY the registered id. The route must expand it to the sibling
    // (alias set from the session index) and converge both buckets, otherwise
    // s-lower stays invisible.
    const refreshed = await postJson<{ refreshed: boolean; inserted?: number; removed?: number }>(
      `/api/v1/workspaces/${encodeURIComponent(typedId)}/refresh`,
      undefined,
    );
    expect(refreshed.body.code).toBe(0);
    expect(refreshed.body.data.inserted).toBeGreaterThanOrEqual(2);

    const listed = await getJson<PageWire>(`/api/v1/sessions?workspace_id=${encodeURIComponent(typedId)}`);
    expect(listed.body.code).toBe(0);
    expect(listed.body.data.items.map((s) => s.id).toSorted()).toEqual(['s-lower', 's-typed']);
  });
});