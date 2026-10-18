// apps/mirri-web/test/fork-streaming.e2e.test.ts
// Fork-then-stream regression e2e: the REAL rust `mirri serve` runs real
// turns against a scripted FakeProviderServer, and the REAL WsRpcApi client
// layer (the same code path the Vue composable drives: fork → snapshot →
// seed → subscribe-with-cursor → submitPrompt) must stream the forked
// session's assistant reply live. The server-side relay is covered by the
// rust suite (mirri-server ws_rpc_tests); this suite pins the CLIENT-side
// wiring — the post-fork subscribe/cursor/projector chain — which is where
// "fork 后发送消息，后台正常，但 Web UI 不实时显示、刷新后才出现"
// lived.
//
// Escape hatch: set MIRRICODE_E2E_SKIP_RUST=1 to skip the whole suite for
// environments without the prebuilt `rust/target/debug/mirri` binary.

import { execFile, spawn, type ChildProcess } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdir, mkdtemp, rm } from 'node:fs/promises';
import { createServer as createNetServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';

import { createFakeProviderServer, type FakeProviderServer } from '@mirri-ai/e2e-harness';

import { WsRpcApi, buildRpcWsUrl } from '../src/api/daemon/ws-rpc-api';
import type { AppEvent } from '../src/api/types';

const execFileAsync = promisify(execFile);

const REPO_ROOT = resolve(fileURLToPath(new URL('../../../', import.meta.url)));
const RUST_BIN = join(REPO_ROOT, 'rust', 'target', 'debug', 'mirri');

const skipRust = process.env.MIRRICODE_E2E_SKIP_RUST === '1';

/** Poll the engine's /api/v1/healthz until it answers (or time out). */
async function waitForHealthz(port: number, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let lastError: unknown = undefined;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`http://127.0.0.1:${port}/api/v1/healthz`);
      if (response.ok) return;
      lastError = new Error(`healthz answered ${response.status}`);
    } catch (error) {
      lastError = error;
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(`rust engine healthz timed out after ${timeoutMs}ms: ${String(lastError)}`);
}

/** Grab a free TCP port (bind port 0, read the assigned port, close). */
async function getFreePort(): Promise<number> {
  const netServer = createNetServer();
  await new Promise<void>((res, rej) => {
    netServer.once('error', rej);
    netServer.listen(0, '127.0.0.1', () => res());
  });
  const address = netServer.address();
  const port = typeof address === 'object' && address !== null ? address.port : 0;
  await new Promise<void>((res) => netServer.close(() => res()));
  return port;
}

/** Seed config.toml + spawn the prebuilt rust engine (mirrors the
 * snapshot-contract e2e's spawnRustServer). */
async function spawnRustServer(opts: {
  port: number;
  homeDir: string;
  workDir: string;
  fakeBaseUrl: string;
}): Promise<{ child: ChildProcess }> {
  if (!existsSync(RUST_BIN)) {
    await execFileAsync('cargo', ['build', '-p', 'mirri-cli'], {
      cwd: join(REPO_ROOT, 'rust'),
      timeout: 600_000,
      maxBuffer: 32 * 1024 * 1024,
    });
  }
  await mkdir(opts.homeDir, { recursive: true });
  const configToml = `
[providers.test]
type = "openai"
api_key = "sk-test"
base_url = "${opts.fakeBaseUrl}"

[models."test/model"]
provider = "test"
model = "gpt-test"
max_context_size = 128000
`;
  const { writeFile } = await import('node:fs/promises');
  await writeFile(join(opts.homeDir, 'config.toml'), configToml, 'utf8');

  const child = spawn(
    RUST_BIN,
    [
      'serve',
      '--host',
      '127.0.0.1',
      '--port',
      String(opts.port),
      '--workspace',
      opts.workDir,
      '--log-level',
      'warn',
      '--dangerous-bypass-auth',
    ],
    {
      stdio: ['ignore', 'pipe', 'pipe'],
      env: {
        ...process.env,
        MIRRICODE_HOME: opts.homeDir,
        OPENAI_API_KEY: 'dummy',
        OPENAI_BASE_URL: opts.fakeBaseUrl,
      },
    },
  );
  let stderr = '';
  child.stderr?.on('data', (chunk: Buffer) => {
    stderr = (stderr + chunk.toString()).slice(-4000);
  });
  try {
    await waitForHealthz(opts.port, 30_000);
  } catch (error) {
    child.kill('SIGKILL');
    throw new Error(`rust engine failed to boot: ${stderr}\n${String(error)}`, { cause: error });
  }
  return { child };
}

/** Politely stop the rust engine via /api/v1/shutdown, falling back to SIGKILL. */
async function stopRustServer(proc: { child: ChildProcess }, base: string): Promise<void> {
  try {
    await fetch(`${base}/api/v1/shutdown`, {
      method: 'POST',
      signal: AbortSignal.timeout(5000),
    });
  } catch {
    // fall through to the kill below
  }
  await Promise.race([
    new Promise<void>((r) => proc.child.once('exit', () => r())),
    new Promise<void>((r) => setTimeout(r, 5000)),
  ]);
  if (proc.child.exitCode === null) {
    proc.child.kill('SIGKILL');
    await Promise.race([
      new Promise<void>((r) => proc.child.once('exit', () => r())),
      new Promise<void>((r) => setTimeout(r, 2000)),
    ]);
  }
}

describe.skipIf(skipRust)('forked session streams live over the WsRpcApi client layer', () => {
  let fakeProvider: FakeProviderServer | undefined;
  let rust: { child: ChildProcess } | undefined;
  let base = '';
  let homeDir = '';
  let workDir = '';

  beforeAll(async () => {
    fakeProvider = await createFakeProviderServer();
    // Turn 1 (source session): plain text reply, so the fork carries history.
    fakeProvider.nextText('First reply');
    // Turn 2 (forked session): the reply the fork's live turn must stream.
    fakeProvider.nextText('Hello from the fork');

    const dir = await mkdtemp(join(tmpdir(), 'mirri-fork-streaming-'));
    homeDir = join(dir, 'mirri-home');
    workDir = join(dir, 'workspace');
    await mkdir(workDir, { recursive: true });

    const port = await getFreePort();
    base = `http://127.0.0.1:${port}`;
    rust = await spawnRustServer({
      port,
      homeDir,
      workDir,
      fakeBaseUrl: fakeProvider.baseUrl,
    });
  }, 120_000);

  afterAll(async () => {
    if (rust !== undefined) {
      await stopRustServer(rust, base);
      rust = undefined;
    }
    if (fakeProvider !== undefined) {
      // Bound the close: an engine that died mid-stream can leave the
      // provider's socket close callback waiting forever.
      await Promise.race([
        fakeProvider.close(),
        new Promise<void>((r) => setTimeout(r, 5000)),
      ]);
      fakeProvider = undefined;
    }
    if (homeDir !== '') {
      await rm(join(homeDir, '..'), { recursive: true, force: true });
      homeDir = '';
    }
  }, 60_000);

  it('should stream the forked session turn live when the client follows the post-fork flow (fork → snapshot → seed → subscribe → prompt)', async () => {
    const api = new WsRpcApi(buildRpcWsUrl(base), base, undefined);
    await api.connectTransport();
    try {
      // Given a session with one completed turn (history for the fork)
      const session = await api.createSession({
        title: 'fork-streaming',
        model: 'test/model',
      });
      await api.submitPrompt(session.id, {
        content: [{ type: 'text', text: 'hi' }],
        model: 'test/model',
      });
      await waitForSessionIdle(api, session.id);

      // And the session is forked (the web UI's fork action)
      const forked = await api.forkSession(session.id);
      expect(forked.id).toMatch(/^session-/);
      expect(forked.id).not.toBe(session.id);

      // And the client runs the exact composable post-fork flow: bind event
      // handlers, snapshot, seed, subscribe at the snapshot cursor
      const events: Array<{ event: AppEvent; sessionId: string; seq: number }> = [];
      const errors: Array<[number, string, boolean]> = [];
      const resyncs: Array<[string, number]> = [];
      const conn = api.connectEvents({
        onEvent: (event, meta) => events.push({ event, sessionId: meta.sessionId, seq: meta.seq }),
        onResync: (sessionId, currentSeq) => resyncs.push([sessionId, currentSeq]),
        onError: (code, msg, fatal) => errors.push([code, msg, fatal]),
        onConnectionChange: () => {},
        onTerminalOutput: () => {},
        onTerminalExit: () => {},
      });
      const snap = await api.getSessionSnapshot(forked.id);
      conn.seedSnapshot(forked.id, snap);
      conn.subscribe(forked.id, { seq: snap.asOfSeq, epoch: snap.epoch });

      // When a prompt is sent to the forked session (fire mode, like the UI)
      const submitted = await api.submitPrompt(forked.id, {
        content: [{ type: 'text', text: 'hello from the fork' }],
        model: 'test/model',
      });
      conn.bindNextPromptId(forked.id, submitted.promptId);

      // Then the fork's assistant text streams live to the client — the
      // accumulated text of the turn's delta/update events for the forked
      // session reaches the full reply without any refresh/snapshot reload
      const streamed = await collectStreamedText(events, forked.id, 'Hello from the fork', 10_000);
      expect(errors).toEqual([]);
      expect(streamed).toBe('Hello from the fork');
      conn.unsubscribe(forked.id);
    } finally {
      api.close();
    }
  }, 60_000);
});

/** Poll the session's message list until the turn's assistant reply lands
 * (the settle signal for a completed fire-mode turn). */
async function waitForSessionIdle(api: WsRpcApi, sessionId: string): Promise<void> {
  const deadline = Date.now() + 10_000;
  for (;;) {
    const page = await api.listMessages(sessionId, { pageSize: 50 });
    if (page.items.some((m) => m.role === 'assistant')) return;
    if (Date.now() > deadline) {
      throw new Error('source session turn did not complete within 10s');
    }
    await new Promise((r) => setTimeout(r, 200));
  }
}

/** Accumulate the assistant text carried by the fork's live events. */
function collectStreamedText(
  events: Array<{ event: AppEvent; sessionId: string; seq: number }>,
  sessionId: string,
  expected: string,
  timeoutMs: number,
): Promise<string> {
  return new Promise<string>((resolve, reject) => {
    const started = Date.now();
    const timer = setInterval(() => {
      const text = assistantTextOf(events, sessionId);
      if (text.includes(expected)) {
        clearInterval(timer);
        resolve(text);
      } else if (Date.now() - started > timeoutMs) {
        clearInterval(timer);
        reject(
          new Error(
            `forked session did not stream live within ${timeoutMs}ms; ` +
              `streamed=${JSON.stringify(text)}; eventTypes=${events
                .filter((e) => e.sessionId === sessionId)
                .map((e) => e.event.type)
                .join(',')}`,
          ),
        );
      }
    }, 100);
  });
}

/** Fold the forked session's live events into the assistant text so far. */
function assistantTextOf(
  events: Array<{ event: AppEvent; sessionId: string; seq: number }>,
  sessionId: string,
): string {
  const parts: string[] = [];
  for (const { event, sessionId: sid } of events) {
    if (sid !== sessionId) continue;
    if (event.type === 'assistantDelta' && typeof event.delta.text === 'string') {
      parts.push(event.delta.text);
    } else if (event.type === 'messageUpdated' || event.type === 'messageCreated') {
      const message = (event as { message?: { role?: string; content?: unknown } }).message;
      if (
        message !== undefined &&
        message.role === 'assistant' &&
        Array.isArray(message.content)
      ) {
        for (const part of message.content as Array<{ type?: string; text?: string }>) {
          if (part.type === 'text' && typeof part.text === 'string') parts.push(part.text);
        }
      }
    }
  }
  return parts.join('');
}
