/**
 * Server e2e: subagent events forwarded over WebSocket.
 *
 * Boots a real v1 server with a FakeProviderServer as the LLM backend,
 * creates a session via REST, subscribes to events via WebSocket, and
 * submits a prompt that triggers a subagent spawn. Asserts that the
 * full subagent event sequence is forwarded over WS:
 *
 *   subagent.spawned → subagent.started → (subagent turn) → subagent.completed
 *
 * This validates the server→client event contract that the web UI's
 * agentEventProjector consumes.
 */

import { mkdtempSync, rmSync, writeFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { WebSocket } from 'ws';
import { pino } from 'pino';

import { createFakeProviderServer, type FakeProviderServer } from '@mirri-ai/e2e-harness';

import { startServer, type RunningServer } from '../src';
import { fixedTokenAuth } from './helpers/serverHarness';

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
  payload?: { type: string; [key: string]: unknown };
  [key: string]: unknown;
}

async function openSubscriber(
  address: string,
  sessionId: string,
): Promise<{ ws: WebSocket; frames: WsFrame[] }> {
  const wsUrl = address.replace('http://', 'ws://') + '/api/v1/ws';
  const frames: WsFrame[] = [];

  const waitFor = (pred: (f: WsFrame) => boolean, timeoutMs = 5000): Promise<WsFrame> => {
    const start = Date.now();
    return new Promise((resolve, reject) => {
      const check = (): void => {
        const hit = frames.find(pred);
        if (hit) { resolve(hit); return; }
        if (Date.now() - start > timeoutMs) {
          reject(new Error(`waitFor timed out; frames: ${frames.map((f) => f.type).join(', ')}`));
          return;
        }
        setTimeout(check, 10);
      };
      check();
    });
  };

  const ws = await new Promise<WebSocket>((resolve, reject) => {
    const sock = new WebSocket(wsUrl, [`mirri-code.bearer.${TOKEN}`], {
      headers: { Authorization: `Bearer ${TOKEN}` },
    });
    sock.on('message', (data) => {
      try {
        frames.push(JSON.parse(wsDataToString(data)) as WsFrame);
      } catch {
        // ignore
      }
    });
    sock.once('open', () => resolve(sock));
    sock.once('error', reject);
  });

  // Wait for server_hello, then send client_hello with subscription
  await waitFor((f) => f.type === 'server_hello');
  ws.send(JSON.stringify({
    type: 'client_hello',
    id: 'h1',
    payload: { client_id: 'test', subscriptions: [sessionId] },
  }));
  await waitFor((f) => f.type === 'ack' && f['id'] === 'h1');

  return { ws, frames };
}

function waitForFrame(
  frames: WsFrame[],
  payloadType: string,
  timeoutMs = 30_000,
): Promise<WsFrame> {
  return new Promise((resolve, reject) => {
    const check = (): void => {
      const found = frames.find((f) => f.payload?.type === payloadType);
      if (found) {
        resolve(found);
        return;
      }
      setTimeout(check, 100);
    };
    setTimeout(() => {
      reject(new Error(`Timed out waiting for WS frame "${payloadType}" after ${timeoutMs}ms`));
    }, timeoutMs);
    check();
  });
}

describe('Server WS e2e: subagent events', () => {
  let fakeProvider: FakeProviderServer;
  let server: RunningServer;
  let homeDir: string;
  let workDir: string;
  const tempDirs: string[] = [];

  beforeEach(async () => {
    fakeProvider = await createFakeProviderServer();

    homeDir = mkdtempSync(join(tmpdir(), 'mirri-server-subagent-home-'));
    workDir = mkdtempSync(join(tmpdir(), 'mirri-server-subagent-work-'));
    tempDirs.push(homeDir, workDir);

    // Write config.toml pointing the provider at the fake server
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

    const lockPath = join(mkdtempSync(join(tmpdir(), 'mirri-server-subagent-lock-')), 'lock');
    tempDirs.push(lockPath);

    server = await startServer({
      host: '127.0.0.1',
      port: 0,
      lockPath,
      serviceOverrides: [fixedTokenAuth(TOKEN)],
      logger: pino({ level: 'silent' }),
      coreProcessOptions: { homeDir },
    });
  });

  afterEach(async () => {
    try {
      await server.close();
    } catch {
      // best-effort
    }
    await fakeProvider.close();
    for (const dir of tempDirs.splice(0)) {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it('should forward subagent.spawned → started → completed over WebSocket', async () => {
    // Main agent calls the Agent tool to spawn a subagent
    fakeProvider.nextToolCall('Agent', {
      prompt: 'Write a hello world function',
      description: 'Delegate coding task to subagent',
    });
    // Subagent turns (may run multiple — init + task)
    for (let i = 0; i < 5; i++) {
      fakeProvider.nextText('I have created the hello world function.');
    }
    // Main agent final response
    for (let i = 0; i < 5; i++) {
      fakeProvider.nextText('The subagent has completed the task successfully.');
    }

    const baseUrl = server.address;

    // 1. Create session via REST
    const createRes = await fetch(`${baseUrl}/api/v1/sessions`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        Authorization: `Bearer ${TOKEN}`,
      },
      body: JSON.stringify({ metadata: { cwd: workDir } }),
    });
    expect(createRes.status).toBeLessThan(300);
    const sessionBody = (await createRes.json()) as { data: { id: string } };
    const sessionId = sessionBody.data.id;
    expect(sessionId).toBeTruthy();

    // 2. Subscribe to WS events
    const { ws, frames } = await openSubscriber(baseUrl, sessionId);

    try {
      // 3. Submit prompt via REST
      const promptRes = await fetch(`${baseUrl}/api/v1/sessions/${sessionId}/prompts`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          Authorization: `Bearer ${TOKEN}`,
        },
        body: JSON.stringify({ content: [{ type: 'text', text: 'Please delegate a coding task to a subagent' }] }),
      });
      expect(promptRes.status).toBeLessThan(300);

      // 4. Wait for subagent events over WS
      const spawnedFrame = await waitForFrame(frames, 'subagent.spawned');
      expect(spawnedFrame.payload?.type).toBe('subagent.spawned');
      expect(spawnedFrame.session_id).toBe(sessionId);
      expect(spawnedFrame.payload?.['subagentId']).toBeTruthy();
      expect(spawnedFrame.payload?.['subagentName']).toBeTruthy();

      const startedFrame = await waitForFrame(frames, 'subagent.started');
      expect(startedFrame.payload?.type).toBe('subagent.started');

      const completedFrame = await waitForFrame(frames, 'subagent.completed');
      expect(completedFrame.payload?.type).toBe('subagent.completed');
      expect(completedFrame.payload?.['subagentId']).toBe(spawnedFrame.payload?.['subagentId']);
      expect(completedFrame.payload?.['resultSummary']).toBeTruthy();

      // Verify that subagent turn events are also forwarded
      const subagentId = spawnedFrame.payload?.['subagentId'] as string;
      const subagentTurnStarted = frames.find(
        (f) => f.payload?.type === 'turn.started' && f.payload?.['agentId'] === subagentId,
      );
      expect(subagentTurnStarted).toBeDefined();

      // Main agent turn.ended should also be forwarded
      await waitForFrame(frames, 'turn.ended', 60_000);

      // Verify fake provider received real HTTP requests
      expect(fakeProvider.requests.length).toBeGreaterThanOrEqual(3);
    } finally {
      ws.close();
    }
  });
});
