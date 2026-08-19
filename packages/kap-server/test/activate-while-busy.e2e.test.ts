/**
 * E2E (real kap-server + real engine, ONLY the LLM API mocked):
 * activating a skill while a turn is running must be accepted immediately
 * (`queued`) instead of holding the HTTP response until the turn completes —
 * the 30s web timeout that surfaces as a misleading "cannot connect to the
 * Mirri daemon" warning.
 *
 * RED phase: with the current engine, the activate call only settles after the
 * running turn finishes, so the short-Timeboxed assertion below fails.
 * GREEN phase: the POST returns `{ status: 'queued', prompt_id }` in time and
 * the skill turn actually starts after the current turn completes.
 *
 * Wiring: real kap-server (config → FakeProviderServer), real engine, no
 * browser — pure HTTP against /api/v1.
 */
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterAll, beforeAll, describe, expect, it } from 'vitest';

import { startReadyServer, type RunningServer } from './helpers/startReadyServer';
import { authHeaders } from './helpers/auth';
import { TEST_HOST_IDENTITY } from './helpers/hostIdentity';
import {
  createFakeProviderServer,
  type FakeProviderServer,
} from '../../e2e-harness/src/fake-provider-server';

interface Envelope<T> {
  code: number;
  msg: string;
  data: T;
  request_id: string;
}

interface SessionWire {
  id: string;
  busy: boolean;
  main_turn_active: boolean;
  message_count: number;
  last_turn_reason?: 'completed' | 'cancelled' | 'failed';
}

interface SkillDescriptorWire {
  name: string;
  description: string;
  path: string;
  source: 'project' | 'user' | 'extra' | 'builtin';
  type?: string;
}

describe('skill activation while a turn is running (e2e, LLM mocked)', () => {
  let fakeProvider: FakeProviderServer;
  let server: RunningServer;
  let home: string;
  let base: string;
  let sessionId: string;
  let skillName: string;

  const SLOW_TURN_MS = 4_000;
  const SHORT_TIMEOUT_MS = 2_500;

  async function postJson<T>(path: string, body?: unknown, signal?: AbortSignal): Promise<{ status: number; body: Envelope<T> }> {
    const hasBody = body !== undefined;
    const res = await fetch(`${base}${path}`, {
      method: 'POST',
      headers: authHeaders(server, hasBody ? { 'content-type': 'application/json' } : {}),
      body: hasBody ? JSON.stringify(body) : undefined,
      signal,
    } as never);
    return { status: res.status, body: (await res.json()) as Envelope<T> };
  }

  async function getJson<T>(path: string): Promise<{ status: number; body: Envelope<T> }> {
    const res = await fetch(`${base}${path}`, {
      headers: authHeaders(server),
    } as never);
    return { status: res.status, body: (await res.json()) as Envelope<T> };
  }

  async function waitFor<T>(fn: () => Promise<T | undefined>, capMs: number): Promise<T> {
    const deadline = Date.now() + capMs;
    for (;;) {
      const value = await fn();
      if (value !== undefined) return value;
      if (Date.now() > deadline) throw new Error('waitFor timed out');
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  }

  async function sessionBusy(): Promise<boolean> {
    const { body } = await getJson<SessionWire>(`/api/v1/sessions/${sessionId}`);
    return body.data.busy;
  }

  beforeAll(async () => {
    fakeProvider = await createFakeProviderServer();
    home = await mkdtemp(join(tmpdir(), 'mirri-activate-while-busy-'));
    await writeFile(
      join(home, 'config.toml'),
      [
        'default_model = "fake-model"',
        '',
        '[providers.local]',
        'type = "openai"',
        'api_key = "sk-test"',
        `base_url = "${fakeProvider.baseUrl}/v1"`,
        '',
        '[models.fake-model]',
        'provider = "local"',
        'model = "fake-model"',
        'max_context_size = 262144',
        '',
      ].join('\n'),
      'utf8',
    );

    server = await startReadyServer({
      hostIdentity: TEST_HOST_IDENTITY,
      host: '127.0.0.1',
      port: 0,
      homeDir: home,
      logLevel: 'silent',
    });
    base = `http://127.0.0.1:${server.port}`;

    const session = await postJson<SessionWire>('/api/v1/sessions', {
      metadata: { cwd: home },
    });
    sessionId = session.body.data.id;

    // Discover a real user-activatable skill from the session catalog
    // (builtins are registered; pick the first 'inline'/'flow' skill).
    const listed = await waitFor(async () => {
      const { body } = await getJson<{ skills: SkillDescriptorWire[] }>(
        `/api/v1/sessions/${sessionId}/skills`,
      );
      const name = body.data.skills.find(
        (s) => s.type === 'inline' || s.type === 'flow' || s.type === undefined,
      )?.name;
      return name === undefined ? undefined : name;
    }, 10_000);
    skillName = listed;
    expect(skillName.length).toBeGreaterThan(0);
  });

  afterAll(async () => {
    try {
      if (server !== undefined) await server.close();
    } catch {
      /* best-effort */
    }
    await fakeProvider?.close();
    if (home) await rm(home, { recursive: true, force: true });
  });

  it('should accept the activation immediately while the turn is running and run the skill afterwards', async () => {
    // Script a SLOW first turn (LLM response delayed past the short timeout),
    // then a normal response for the queued skill turn.
    fakeProvider.setStreamDelay(SLOW_TURN_MS);
    fakeProvider.nextText('first turn done');
    fakeProvider.nextText('skill turn done');

    // Start a prompt and wait until the LLM request is actually in flight
    // (turn active) — only then is the session truly busy.
    await postJson(`/api/v1/sessions/${sessionId}/prompts`, {
      content: [{ type: 'text', text: 'run the long first turn' }],
    });
    await waitFor(async () => (fakeProvider.requests.length >= 1 ? true : undefined), 10_000);

    // 2. Activate a skill while the turn is running. Must come back quickly
    //    with a queued receipt instead of hanging until the turn completes.
    const { status, body } = await postJson<{ activated: true; status: string; prompt_id: string }>(
      `/api/v1/sessions/${sessionId}/skills/${encodeURIComponent(skillName)}:activate`,
      { args: 'queue the activation' },
      AbortSignal.timeout(SHORT_TIMEOUT_MS),
    ).catch((error: unknown) => {
      throw new Error(
        `skill activation was not acknowledged within ${SHORT_TIMEOUT_MS}ms (server held the request past the turn) — ${String(error)}`,
      );
    });
    expect(status).toBe(200);
    expect(body.code).toBe(0);
    expect(body.data.activated).toBe(true);
    expect(['queued', 'running']).toContain(body.data.status);
    expect(typeof body.data.prompt_id).toBe('string');

    // 3. The daemon prompt queue is the source of truth: while the first turn is
    //    still running, the parked activation is listed as queued there (this
    //    is what lets a reload re-render the queue area via GET /prompts).
    fakeProvider.setStreamDelay(0);
    const queueWhileBusy = await getJson<{
      active: { prompt_id: string } | null;
      queued: { prompt_id: string }[];
    }>(`/api/v1/sessions/${sessionId}/prompts`);
    expect(queueWhileBusy.body.code).toBe(0);
    expect(
      queueWhileBusy.body.data.queued.some((q) => q.prompt_id === body.data.prompt_id),
    ).toBe(true);

    // 4. The skill must actually run once the long turn finishes: the engine
    //    then issues a second LLM request (the skill's own prompt).
    const sawSecondLlmRequest = await waitFor(async () => {
      const request = fakeProvider.requests[1];
      if (request === undefined) return undefined;
      const text = typeof request.bodyText === 'string' ? request.bodyText : '';
      return text.includes(skillName) ? true : undefined;
    }, 20_000);
    expect(sawSecondLlmRequest).toBe(true);

    // 5. After the skill turn runs to completion the parked row is gone from
    //    the daemon queue, and the session returns to idle.
    await waitFor(async () => (await sessionBusy()) ? undefined : true, 20_000);
    const queueAfter = await getJson<{ queued: { prompt_id: string }[] }>(
      `/api/v1/sessions/${sessionId}/prompts`,
    );
    expect(
      queueAfter.body.data.queued.some((q) => q.prompt_id === body.data.prompt_id),
    ).toBe(false);
  });

  it('should reject an unknown skill name synchronously even while a turn is running', async () => {
    const { status, body } = await postJson<never>(
      `/api/v1/sessions/${sessionId}/skills/needs-more-sync:activate`,
    );
    // Still an envelope error (not a hang): unknown skill must not enter the queue.
    expect(status).toBe(200);
    expect(body.code).not.toBe(0);
  });
});