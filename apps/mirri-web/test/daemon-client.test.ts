// apps/mirri-web/test/daemon-client.test.ts
// DaemonMirriWebApi.getSessionGoal — wire → app mapping of GET /sessions/{id}/goal:
// a present snapshot, explicit null (no active goal), and the request URL.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { DaemonMirriWebApi } from '../src/api/daemon/client';

function envelope(data: unknown): Response {
  return new Response(JSON.stringify({ code: 0, msg: '', data }), {
    status: 200,
    headers: { 'content-type': 'application/json' },
  });
}

const WIRE_GOAL = {
  goalId: 'goal_1',
  objective: 'fix all lint warnings',
  status: 'active',
  turnsUsed: 1,
  tokensUsed: 0,
  wallClockMs: 0,
  budget: {
    tokenBudget: null,
    turnBudget: null,
    wallClockBudgetMs: null,
    remainingTokens: null,
    remainingTurns: null,
    remainingWallClockMs: null,
    tokenBudgetReached: false,
    turnBudgetReached: false,
    wallClockBudgetReached: false,
    overBudget: false,
  },
};

function createApi(): DaemonMirriWebApi {
  return new DaemonMirriWebApi({
    serverHttpUrl: 'http://daemon.test',
    clientId: 'web_test',
    clientName: 'test',
    clientVersion: '0.0.0',
    clientUiMode: 'test',
  });
}

describe('DaemonMirriWebApi.getSessionGoal', () => {
  beforeEach(() => {
    vi.stubGlobal('fetch', vi.fn());
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('should map a present goal snapshot', async () => {
    vi.mocked(fetch).mockResolvedValue(envelope(WIRE_GOAL));
    const goal = await createApi().getSessionGoal('sess_1');
    expect(goal?.objective).toBe('fix all lint warnings');
    expect(goal?.status).toBe('active');
    expect(goal?.turnsUsed).toBe(1);
  });

  it('should map null to null (no active goal)', async () => {
    vi.mocked(fetch).mockResolvedValue(envelope(null));
    const goal = await createApi().getSessionGoal('sess_1');
    expect(goal).toBeNull();
  });

  it('should request the session goal endpoint', async () => {
    vi.mocked(fetch).mockResolvedValue(envelope(null));
    await createApi().getSessionGoal('sess_42');
    expect(vi.mocked(fetch).mock.calls[0]?.[0]).toBe(
      'http://daemon.test/api/v1/sessions/sess_42/goal',
    );
  });
});

describe('DaemonMirriWebApi.listToolsCatalog', () => {
  beforeEach(() => {
    vi.stubGlobal('fetch', vi.fn());
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('should fall back to /tools/all when the v1 backend 404s /tools/catalog', async () => {
    // The v1 backend replies to unknown routes with Fastify's default 404
    // body — valid JSON but not a daemon envelope (no `code` field).
    const notFound = new Response(
      JSON.stringify({ message: 'Route GET:/api/v1/tools/catalog not found', error: 'Not Found', statusCode: 404 }),
      { status: 404, headers: { 'content-type': 'application/json' } },
    );
    vi.mocked(fetch)
      .mockResolvedValueOnce(notFound)
      .mockResolvedValueOnce(
        envelope({
          tools: [
            { name: 'Read', description: 'Read a file.', input_schema: null, source: 'builtin' },
          ],
        }),
      );

    const tools = await createApi().listToolsCatalog();
    expect(tools).toHaveLength(1);
    expect(tools[0]).toMatchObject({ name: 'Read', description: 'Read a file.', source: 'builtin' });
    expect(vi.mocked(fetch).mock.calls[0]?.[0]).toBe('http://daemon.test/api/v1/tools/catalog');
    expect(vi.mocked(fetch).mock.calls[1]?.[0]).toBe('http://daemon.test/api/v1/tools/all');
  });

  it('should rethrow when the catalog request fails with a non-404 error', async () => {
    vi.mocked(fetch).mockResolvedValue(
      new Response(
        JSON.stringify({ code: 50001, msg: 'internal error', data: null }),
        { status: 200, headers: { 'content-type': 'application/json' } },
      ),
    );
    await expect(createApi().listToolsCatalog()).rejects.toMatchObject({
      name: 'DaemonApiError',
      code: 50001,
    });
  });
});
