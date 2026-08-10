/**
 * v2 (kap-server) e2e: REAL ws contract for subagent completion events.
 *
 * WHY THIS TEST EXISTS
 * -------------------
 * The web UI (which runs ONLY on v2 / kap-server) showed subagents stuck in
 * "Running" well after they had completed. The v1 full-chain test passes —
 * but the web UI never talks to v1. This test drives the REAL v2 engine
 * through a REAL kap-server with a FakeProviderServer LLM, and asserts the
 * WebSocket session_event contract that the web client's agentEventProjector
 * depends on:
 *
 *   - every subagent.* frame carries `subagentId` INSIDE payload
 *     (the web projector reads payload.subagentId — no id = task never updates)
 *   - `subagent.completed` carries `resultSummary` and a subagentId matching
 *     the one from `subagent.spawned`
 *   - subagent.* events are durable (advancing seq), so a resubscribing client
 *     replays them
 *   - the full lifecycle spawned → started → completed arrives in order
 *
 * The browser-side projector store assertion lives in apps/mirri-web
 * (subagent-fullchain.e2e.test.ts, v1 server) — that test consumes this
 * exact envelope shape. Keep the field names here and there in sync: if the
 * backend renames `subagentId` inside the ws payload, this test fails.
 */

import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { WebSocket } from 'ws';

import {
  createFakeProviderServer,
  type FakeProviderServer,
} from '@mirri-ai/e2e-harness';

import { startReadyServer, type RunningServer } from './helpers/startReadyServer';
import { TEST_HOST_IDENTITY } from './helpers/hostIdentity';
import { authHeaders } from './helpers/auth';

function wsDataToString(data: unknown): string {
  if (typeof data === 'string') return data;
  if (data instanceof Buffer) return data.toString('utf8');
  if (data instanceof ArrayBuffer) return new TextDecoder().decode(data);
  if (Array.isArray(data)) return Buffer.concat(data).toString('utf8');
  return String(data);
}

interface WsFrame {
  type: string;
  seq?: number;
  session_id?: string;
  payload?: {
    type?: string;
    subagentId?: string;
    subagentName?: string;
    resultSummary?: string;
    [key: string]: unknown;
  };
  [key: string]: unknown;
}

function waitFor(
  frames: WsFrame[],
  pred: (f: WsFrame) => boolean,
  timeoutMs = 90_000,
): Promise<WsFrame> {
  const start = Date.now();
  return new Promise((resolve, reject) => {
    const check = (): void => {
      const hit = frames.find(pred);
      if (hit) { resolve(hit); return; }
      if (Date.now() - start > timeoutMs) {
        const types = frames.map((f) => `${f.type}${f.payload?.type ? `:${f.payload.type}` : ''}`).join(', ');
        reject(new Error(`waitFor timed out; frames: ${types}`));
        return;
      }
      setTimeout(check, 50);
    };
    check();
  });
}

describe('Subagent v2 full-chain: engine → WS → projector → reducer', () => {
  let fakeProvider: FakeProviderServer;
  let server: RunningServer;
  let homeDir: string;
  let baseUrl: string;
  const tempDirs: string[] = [];

  beforeEach(async () => {
    fakeProvider = await createFakeProviderServer();
    homeDir = await mkdtemp(join(tmpdir(), 'mirri-kap-chain-'));
    tempDirs.push(homeDir);
    const workDir = await mkdtemp(join(tmpdir(), 'mirri-kap-chain-work-'));
    tempDirs.push(workDir);

    const config = `
default_model = "fake-model"

[providers.local]
type = "openai"
api_key = "sk-test"
base_url = "${fakeProvider.baseUrl}/v1"

[models.fake-model]
provider = "local"
model = "fake-model"
max_context_size = 262144
`;
    await writeFile(join(homeDir, 'config.toml'), config, 'utf8');

    server = await startReadyServer({
      hostIdentity: TEST_HOST_IDENTITY,
      host: '127.0.0.1',
      port: 0,
      homeDir,
      logLevel: 'silent',
    });
    baseUrl = `http://127.0.0.1:${server.port}`;
  });

  afterEach(async () => {
    try { await server.close(); } catch { /* best-effort */ }
    await fakeProvider.close();
    await new Promise((r) => setTimeout(r, 50));
    for (const dir of tempDirs.splice(0)) {
      await rm(dir, { recursive: true, force: true });
    }
  });

  it('should emit subagent.spawned → started → completed over WS with the payload shape the web projector consumes', async () => {
    // Main agent delegates to a subagent; subagent runs; main finalizes.
    fakeProvider.nextToolCall('Agent', {
      prompt: 'Write a hello world function',
      description: 'Delegate coding task to subagent',
    });
    for (let i = 0; i < 5; i++) fakeProvider.nextText('I have created the hello world function.');
    for (let i = 0; i < 5; i++) fakeProvider.nextText('The subagent completed the task.');

    // 1. Create a session via REST (exactly like the web client).
    const token = server.authTokenService.getToken();
    const createRes = await fetch(`${baseUrl}/api/v1/sessions`, {
      method: 'POST',
      headers: authHeaders(server, { 'Content-Type': 'application/json' }),
      body: JSON.stringify({ metadata: { cwd: homeDir } }),
    });
    const createJson = (await createRes.json()) as { data: { id: string } };
    const sessionId = createJson.data.id;
    expect(sessionId).toBeTruthy();

    // 2. Open WS and subscribe.
    const frames: WsFrame[] = [];
    const ws = await new Promise<WebSocket>((resolve, reject) => {
      const sock = new WebSocket(
        `ws://127.0.0.1:${server.port}/api/v1/ws`,
        [`mirri-code.bearer.${token}`],
      );
      sock.on('message', (data) => {
        let frame: WsFrame;
        try { frame = JSON.parse(wsDataToString(data)) as WsFrame; } catch { return; }

        if (frame.type === 'server_hello') {
          sock.send(JSON.stringify({
            type: 'subscribe',
            id: 's1',
            payload: { session_ids: [sessionId] },
          }));
          return;
        }
        if (frame.type === 'ack') return;
        frames.push(frame);
      });
      sock.once('open', () => resolve(sock));
      sock.once('error', reject);
    });

    try {
      // 3. Submit the prompt (auto-creates main agent + binds the model).
      const promptRes = await fetch(`${baseUrl}/api/v1/sessions/${sessionId}/prompts`, {
        method: 'POST',
        headers: authHeaders(server, { 'Content-Type': 'application/json' }),
        body: JSON.stringify({ content: [{ type: 'text', text: 'please delegate a coding task to a subagent' }] }),
      });
      const promptJson = (await promptRes.json()) as { code: number };
      expect(promptJson.code).toBe(0);

      // 4. Wait for the whole subagent lifecycle over the wire.
      const spawned = await waitFor(frames, (f) => f.payload?.type === 'subagent.spawned');
      await waitFor(frames, (f) => f.payload?.type === 'subagent.started');
      const completed = await waitFor(frames, (f) => f.payload?.type === 'subagent.completed');
      await waitFor(frames, (f) => f.payload?.type === 'turn.ended');

      // ---- REAL WS contract assertions (this is where subagentId shape breaks) ----
      // (a) The frame targets the right session and is durable (has a seq).
      expect(spawned.session_id).toBe(sessionId);
      expect(spawned.seq).toBeGreaterThan(0);
      expect(spawned.payload?.type).toBe('subagent.spawned');
      expect(spawned.payload?.subagentName).toBeTruthy();

      // (b) subagentId must live INSIDE payload — the web client's
      //     agentEventProjector reads p?.subagentId from payload. If the
      //     backend moves it (e.g. renames to agentId or flattens it), the
      //     projector can never find the task and it stays 'running' forever.
      expect(typeof completed.payload?.subagentId).toBe('string');
      expect(completed.payload?.subagentId).toBe(spawned.payload?.subagentId);
      expect(completed.payload?.resultSummary).toBeTruthy();

      // (c) Durable ordering: completed arrives after spawned with a higher seq.
      expect(completed.seq ?? 0).toBeGreaterThan(spawned.seq ?? 0);

      // (d) The completed subagent is a real subagent (not 'main'), so the
      //     UI has exactly the data needed to flip the task to completed.
      expect(completed.payload?.subagentId).not.toBe('main');
      expect(completed.payload?.subagentId!.length).toBeGreaterThan(0);

      // (e) A REST snapshot taken NOW must still report the completed
      //     subagent — this is exactly what a reloading web client consumes
      //     to rebuild the Sub Agent dock. (Reported bug: refresh dropped the
      //     completed task entirely.)
      const snapshotRes = await fetch(`${baseUrl}/api/v1/sessions/${sessionId}/snapshot`, {
        headers: authHeaders(server),
      });
      const snapshotJson = (await snapshotRes.json()) as {
        code: number;
        data?: { subagents?: Array<{ id: string; status?: string; subagent_phase?: string }> };
      };
      expect(snapshotJson.code).toBe(0);
      const subs = snapshotJson.data?.subagents ?? [];
      const completedSub = subs.find((s) => s.id === spawned.payload?.subagentId);
      expect(completedSub).toBeDefined();
      expect(completedSub?.status).toBe('completed');
    } finally {
      ws.close();
    }
  });
});