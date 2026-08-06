/**
 * Kap server end-to-end smoke test — verifies the full lifecycle:
 * boot → healthz → meta (backend: v2) → create session → submit prompt
 * → list messages. No real LLM required; uses a stub provider pointing at
 * an unreachable port so prompt submission is accepted but never completes.
 */

import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import {
  IAgentLifecycleService,
  getLiveSessionById,
} from '@mirri-ai/agent-core-v2';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { type RunningServer, startServer } from '../src/start';
import { authHeaders } from './helpers/auth';
import { TEST_HOST_IDENTITY } from './helpers/hostIdentity';

/** Minimal envelope shape shared by all v2 REST responses. */
interface Envelope<T> {
  code: number;
  msg: string;
  data: T;
  request_id: string;
}

/** Shape returned by POST /api/v1/sessions. */
interface SessionWire { id: string }

/** Shape returned by POST /api/v1/sessions/:id/prompts. */
interface PromptItemWire {
  prompt_id: string;
  user_message_id: string;
  status: string;
  created_at: string;
}

/** Shape returned by GET /api/v1/meta. */
interface MetaWire {
  server_version?: string;
  backend?: 'v1' | 'v2';
}

/** Stub provider config — points at an unreachable port so no real LLM calls. */
const STUB_TOML = [
  'default_model = "stub"',
  '',
  '[providers.stub]',
  'type = "openai"',
  'base_url = "http://127.0.0.1:9999"',
  'api_key = "stub"',
  '',
  '[models.stub]',
  'provider = "stub"',
  'model = "stub"',
  'max_context_size = 1000',
  '',
].join('\n');

describe('server-v2 smoke', () => {
  let server: RunningServer | undefined;
  let home: string | undefined;
  let base: string;

  beforeEach(async () => {
    home = await mkdtemp(join(tmpdir(), 'mirri-server-v2-smoke-'));
    await writeFile(join(home, 'config.toml'), STUB_TOML, 'utf-8');
    server = await startServer({
      hostIdentity: TEST_HOST_IDENTITY,
      host: '127.0.0.1',
      port: 0,
      homeDir: home,
      logLevel: 'silent',
    });
    base = `http://127.0.0.1:${server.port}`;
  });

  afterEach(async () => {
    if (server !== undefined) {
      await server.close();
      server = undefined;
      await new Promise((resolve) => setTimeout(resolve, 25));
    }
    if (home !== undefined) {
      await rm(home, { recursive: true, force: true, maxRetries: 3, retryDelay: 25 } as never);
      home = undefined;
    }
  });

  /** Authenticated JSON call helper. */
  async function call<T>(
    method: 'GET' | 'POST',
    path: string,
    arg?: unknown,
  ): Promise<{ status: number; body: Envelope<T> }> {
    const headers = authHeaders(
      server as RunningServer,
      arg === undefined ? {} : { 'content-type': 'application/json' },
    );
    const init: { method: string; headers: Record<string, string>; body?: string } = {
      method,
      headers,
    };
    if (arg !== undefined) {
      init.body = JSON.stringify(arg);
    }
    const res = await fetch(`${base}${path}`, init as never);
    return { status: res.status, body: (await res.json()) as Envelope<T> };
  }

  /** Create a session via the REST API and return its ID. */
  async function createSession(cwd: string): Promise<string> {
    const res = await fetch(`${base}/api/v1/sessions`, {
      method: 'POST',
      headers: authHeaders(server as RunningServer, { 'content-type': 'application/json' }),
      body: JSON.stringify({ metadata: { cwd } }),
    } as never);
    const body = (await res.json()) as Envelope<SessionWire>;
    expect(body.code).toBe(0);
    return body.data.id;
  }

  /**
   * Create the 'main' agent scope in the session.
   * (server-v2 gap G10: the main agent is not auto-created on session creation.)
   */
  async function createMainAgent(sessionId: string): Promise<void> {
    const session = getLiveSessionById(server!.core.accessor, sessionId);
    if (session === undefined) throw new Error(`session ${sessionId} not found`);
    await session.accessor.get(IAgentLifecycleService).create({ agentId: 'main' });
  }

  it('should boot, report backend v2 on /meta, and accept a prompt submission', async () => {
    // 1. Healthz — unauthenticated, must return 200
    const healthz = await fetch(`${base}/api/v1/healthz`);
    expect(healthz.status).toBe(200);

    // 2. Meta — must report backend: 'v2'
    const { body: metaBody } = await call<MetaWire>('GET', '/api/v1/meta');
    expect(metaBody.code).toBe(0);
    expect(metaBody.data.backend).toBe('v2');

    // 3. Create session
    const sessionId = await createSession(home as string);
    expect(sessionId).toBeTruthy();

    // 4. Create main agent scope
    await createMainAgent(sessionId);

    // 5. Submit prompt — accepted even though the stub provider is unreachable;
    //    the prompt goes to 'running' status and the LLM call will eventually
    //    time out / fail, which is fine for the smoke test.
    const { status: submitStatus, body: submitBody } = await call<PromptItemWire>(
      'POST',
      `/api/v1/sessions/${sessionId}/prompts`,
      { content: [{ type: 'text', text: 'Say hello in exactly one word.' }] },
    );
    expect(submitStatus).toBe(200);
    expect(submitBody.code).toBe(0);
    expect(submitBody.data.prompt_id).toBeTruthy();
    expect(submitBody.data.status).toBe('running');

    // 6. List prompts — verify the response shape is correct. The prompt may
    //    have already errored out (stub provider is unreachable), so we only
    //    assert structural correctness, not that the prompt is still present.
    const { body: listBody } = await call<{ active: PromptItemWire | null; queued: PromptItemWire[] }>(
      'GET',
      `/api/v1/sessions/${sessionId}/prompts`,
    );
    expect(listBody.code).toBe(0);
    expect(listBody.data).toHaveProperty('active');
    expect(Array.isArray(listBody.data.queued)).toBe(true);
  });

  it('should return session-not-found error code for unknown session on prompt submission', async () => {
    const { status, body } = await call<unknown>(
      'POST',
      '/api/v1/sessions/nonexistent-session-id/prompts',
      { content: [{ type: 'text', text: 'hello' }] },
    );
    // v2 uses application-level error codes in the envelope with HTTP 200
    expect(status).toBe(200);
    expect(body.code).toBe(40401);
  });

  it('should include server_version in meta when serverVersion is provided', async () => {
    // Close the default beforeEach server and start one with serverVersion
    if (server !== undefined) {
      await server.close();
      server = undefined;
    }

    const versionedHome = await mkdtemp(join(tmpdir(), 'mirri-server-v2-smoke-ver-'));
    await writeFile(join(versionedHome, 'config.toml'), STUB_TOML, 'utf-8');

    server = await startServer({
      hostIdentity: TEST_HOST_IDENTITY,
      host: '127.0.0.1',
      port: 0,
      homeDir: versionedHome,
      logLevel: 'silent',
      serverVersion: '1.2.3-smoke',
    });
    base = `http://127.0.0.1:${server.port}`;

    // Clean up the extra home dir
    if (home !== undefined) {
      await rm(home, { recursive: true, force: true, maxRetries: 3, retryDelay: 25 } as never);
    }
    home = versionedHome;

    const { body: metaBody } = await call<MetaWire>('GET', '/api/v1/meta');
    expect(metaBody.data.server_version).toBe('1.2.3-smoke');
    expect(metaBody.data.backend).toBe('v2');
  });

  describe('healthz readiness states', () => {
    it('should return code:0 and ready:true after serverReadyPromise resolves', async () => {
      await server!.serverReadyPromise;

      const healthz = await fetch(`${base}/api/v1/healthz`);
      expect(healthz.status).toBe(200);
      const body = (await healthz.json()) as { code: number; data: { ok: boolean; ready: boolean } };
      expect(body.code).toBe(0);
      expect(body.data.ok).toBe(true);
      expect(body.data.ready).toBe(true);
    });

    it('should return code:0 or code:1 on healthz immediately after startServer returns', async () => {
      // Immediately after startServer() returns the server is listening but
      // background startup work may still be in progress. Both code:0 (fast
      // reconcile finished) and code:1 (still running) are valid here.
      const healthz = await fetch(`${base}/api/v1/healthz`);
      expect(healthz.status).toBe(200);
      const body = (await healthz.json()) as { code: number };
      expect(body.code === 0 || body.code === 1).toBe(true);
    });
  });
});
