/**
 * Full-chain e2e: real engine → real server WS → classifyFrame → projector → reducer → store.
 *
 * This test exists because the projector unit tests passed even though the
 * real UI showed subagents stuck in "Running" after they had completed.
 * The projector only proves that GIVEN a correctly-shaped frame, the mapping
 * works. It does NOT prove the real chain delivers that frame correctly.
 *
 * Chain under test (mirrors production ws.ts → client.ts → useMirriWebClient):
 *   real v1 server, real FakeProviderServer LLM, real subagent run
 *   → WebSocket frames with server envelope {type, session_id, payload, seq}
 *   → classifyFrame(type, payload)            (route decision)
 *   → projector.project(type, payload, sessionId)  (raw agent-core → AppEvent)
 *   → reduceAppEvent(state, appEvent, meta)   (store update)
 *   → assert tasksBySession[sessionId] subagent task ends in status 'completed'
 */

import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { pino } from 'pino';
import { WebSocket } from 'ws';

import {
  createFakeProviderServer,
  type FakeProviderServer,
} from '@mirri-ai/e2e-harness';
import type { RunningServer } from '@mirri-ai/server';
import { startServer } from '../../packages/server/src/index';
import { IAuthTokenService } from '../../packages/server/src/services/auth/authTokenService';

import { createAgentProjector, classifyFrame } from '../src/api/daemon/agentEventProjector';
import { createInitialState, reduceAppEvent } from '../src/api/daemon/eventReducer';

const TOKEN = 'test-token';

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
  payload?: Record<string, unknown>;
  [key: string]: unknown;
}

function waitForFrame(
  frames: WsFrame[],
  predicate: (f: WsFrame) => boolean,
  timeoutMs = 60_000,
): Promise<WsFrame> {
  const start = Date.now();
  return new Promise((resolve, reject) => {
    const check = (): void => {
      const hit = frames.find(predicate);
      if (hit) { resolve(hit); return; }
      if (Date.now() - start > timeoutMs) {
        reject(new Error(`Timeout waiting for frame. Got types: ${frames.map((f) => f.type).join(', ')}`));
        return;
      }
      setTimeout(check, 50);
    };
    check();
  });
}

describe('Subagent full-chain e2e: engine → WS → projector → reducer', () => {
  let fakeProvider: FakeProviderServer;
  let server: RunningServer;
  let homeDir: string;
  const tempDirs: string[] = [];

  beforeEach(async () => {
    fakeProvider = await createFakeProviderServer();
    homeDir = mkdtempSync(join(tmpdir(), 'mirri-web-chain-home-'));
    const workDir = mkdtempSync(join(tmpdir(), 'mirri-web-chain-work-'));
    tempDirs.push(homeDir, workDir);

    const configContent = `
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
    writeFileSync(join(homeDir, 'config.toml'), configContent, 'utf8');

    const lockPath = join(mkdtempSync(join(tmpdir(), 'virri-web-chain-lock-')), 'lock');
    tempDirs.push(lockPath);

    server = await startServer({
      host: '127.0.0.1',
      port: 0,
      lockPath,
      serviceOverrides: [[
        IAuthTokenService,
        {
          _serviceBrand: undefined,
          getToken: () => TOKEN,
          isValid: async (candidate: string) => candidate === TOKEN,
        },
      ]],
      logger: pino({ level: 'silent' }),
      coreProcessOptions: { homeDir },
    });
  });

  afterEach(async () => {
    try { await server.close(); } catch { /* best-effort */ }
    await fakeProvider.close();
    for (const dir of tempDirs.splice(0)) {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it('should leave the subagent task in completed state after the real run', async () => {
    // Main agent delegates to a subagent, which runs and completes.
    fakeProvider.nextToolCall('Agent', {
      prompt: 'Write a hello world function',
      description: 'Delegate coding task',
    });
    for (let i = 0; i < 5; i++) fakeProvider.nextText('I have created the function.');
    for (let i = 0; i < 5; i++) fakeProvider.nextText('Done, the subagent finished.');

    // Real chain client.
    const baseUrl = server.address;
    const wsUrl = baseUrl.replace('http://', 'ws://') + '/api/v1/ws';

    // 1. Create a session via REST (the way the web UI does).
    const createRes = await fetch(`${baseUrl}/api/v1/sessions`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${TOKEN}` },
      body: JSON.stringify({ metadata: { cwd: join(homeDir, 'work') } }),
    });
    const createJson = (await createRes.json()) as { data: { id: string } };
    const sessionId = createJson.data.id;

    // 2. Connect WS, subscribe, collect frames.
    const projector = createAgentProjector();
    let store = createInitialState();
    const frames: WsFrame[] = [];

    const ws = await new Promise<WebSocket>((resolve, reject) => {
      const sock = new WebSocket(wsUrl, [`mirri-code.bearer.${TOKEN}`], {
        headers: { Authorization: `Bearer ${TOKEN}` },
      });
      sock.on('message', (data) => {
        let frame: WsFrame;
        try { frame = JSON.parse(wsDataToString(data)) as WsFrame; } catch { return; }

        // Replay the exact frame processing loop from ws.ts.
        if (frame.type === 'server_hello') {
          // Now subscribe.
          sock.send(JSON.stringify({ type: 'client_hello', id: 'h1', payload: { client_id: 'test', subscriptions: [sessionId] } }));
          return;
        }
        if (frame.type === 'ack') return;

        // Durable event envelope → default branch in ws.ts.
        const type = frame.type;
        const decision = classifyFrame(type, frame.payload);

        if (decision.route === 'protocol') {
          // Genuine protocol events are not routed here; nothing subagent-related.
          return;
        }
        if (decision.route === 'agent') {
          const seq = typeof frame.seq === 'number' ? frame.seq : 0;
          const sessionId_ = frame.session_id ?? sessionId;
          const appEvents = projector.project(decision.agentType, frame.payload, sessionId_, { offset: 0 });
          for (const appEvent of appEvents) {
            store = reduceAppEvent(store, appEvent, { sessionId: sessionId_, seq });
          }
        }
        frames.push(frame);
      });
      sock.once('open', () => resolve(sock));
      sock.once('error', reject);
    });

    try {
      // 3. Submit the prompt.
      const promptRes = await fetch(`${baseUrl}/api/v1/sessions/${sessionId}/prompts`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${TOKEN}` },
        body: JSON.stringify({ content: [{ type: 'text', text: 'please delegate a coding task to a subagent' }] }),
      });

      // Wait for the subagent to spawn and complete over the wire.
      await waitForFrame(frames, (f) => f.payload?.type === 'subagent.spawned');
      await waitForFrame(frames, (f) => f.payload?.type === 'subagent.completed');
      // Wait for the main agent turn to end.
      await waitForFrame(frames, (f) => f.payload?.type === 'turn.ended', 60_000);

      // 4. THE assertion that would have caught the reported bug:
      //    after the full chain, the store's subagent task must be completed.
      const tasks = store.tasksBySession[sessionId] ?? [];
      const subagentTasks = tasks.filter((t) => t.kind === 'subagent');
      expect(subagentTasks.length).toBeGreaterThanOrEqual(1);

      const completed = subagentTasks.filter((t) => t.status === 'completed');
      expect(completed.length).toBe(1); // exactly one subagent, now completed

      const running = subagentTasks.filter((t) => t.status === 'running');
      expect(running.length).toBe(0); // ← the bug: tasks stuck in 'running'

      // The completed task should carry the result summary as outputPreview.
      expect(completed[0]?.outputPreview).toBeTruthy();
    } finally {
      ws.close();
    }
  });
});