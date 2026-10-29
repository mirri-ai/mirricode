/**
 * Web UI projector e2e: real rust engine subagent events → projector → task state.
 *
 * Uses the REAL rust `mirri serve` engine against a scripted FakeProviderServer
 * to produce real subagent events (Agent tool call → spawned subagent →
 * streamed output → completion), then observes the AppEvent stream through the
 * REAL WsRpcApi client layer — the same code path the Vue composable drives
 * (the client routes every engine event through agentEventProjector). The
 * projector's taskCreated / taskProgress / taskCompleted AppEvents drive the
 * "Sub Agent" dock panel and AgentDetailPanel in the web UI.
 *
 * Escape hatch: set MIRRICODE_E2E_SKIP_RUST=1 to skip the whole suite for
 * environments without the prebuilt `rust/target/debug/mirri` binary.
 */

import { execFile, spawn, type ChildProcess } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
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

interface TaskState {
  id: string;
  status: string;
  outputChunks: string[];
}

function reduceTaskEvents(events: AppEvent[]): Map<string, TaskState> {
  const tasks = new Map<string, TaskState>();
  for (const event of events) {
    if (event.type === 'taskCreated') {
      const existing = tasks.get(event.task.id) ?? { id: event.task.id, status: '', outputChunks: [] };
      existing.status = event.task.status ?? existing.status;
      tasks.set(event.task.id, existing);
    } else if (event.type === 'taskProgress') {
      const task = tasks.get(event.taskId) ?? { id: event.taskId, status: 'running', outputChunks: [] };
      if (event.outputChunk) {
        task.outputChunks.push(event.outputChunk);
      }
      tasks.set(event.taskId, task);
    } else if (event.type === 'taskCompleted') {
      const task = tasks.get(event.taskId) ?? { id: event.taskId, status: '', outputChunks: [] };
      task.status = event.status;
      tasks.set(event.taskId, task);
    }
  }
  return tasks;
}

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
 * fork-streaming e2e's spawnRustServer). */
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

describe.skipIf(skipRust)('Web UI projector: real rust engine subagent events', () => {
  let fakeProvider: FakeProviderServer | undefined;
  let rust: { child: ChildProcess } | undefined;
  let base = '';
  let homeDir = '';
  let workDir = '';

  beforeAll(async () => {
    fakeProvider = await createFakeProviderServer();
    // Turn 1: the main agent delegates to a subagent via the Agent tool.
    fakeProvider.nextToolCall('Agent', {
      prompt: 'Write a hello world function',
      description: 'Delegate coding task to subagent',
    });
    // The subagent's streamed reply (projected as taskProgress outputChunk).
    for (let i = 0; i < 5; i++) fakeProvider.nextText('I have created the hello world function.');
    // The main agent's final reply after the subagent completes.
    for (let i = 0; i < 5; i++) fakeProvider.nextText('The subagent has completed the task successfully.');

    const dir = await mkdtemp(join(tmpdir(), 'mirri-subagent-projector-'));
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

  it('should project subagent spawned → streamed output → completed into the task lifecycle', async () => {
    const api = new WsRpcApi(buildRpcWsUrl(base), base, undefined);
    await api.connectTransport();
    try {
      const session = await api.createSession({
        title: 'subagent-projector',
        model: 'test/model',
      });

      // Observe the AppEvent stream — the WsRpcApi client routes every
      // engine event through the projector internally (the same path the
      // Vue composable consumes).
      const appEvents: AppEvent[] = [];
      const conn = api.connectEvents({
        onEvent: (event) => {
          appEvents.push(event);
        },
        onResync: () => {},
        onError: () => {},
        onConnectionChange: () => {},
        onTerminalOutput: () => {},
        onTerminalExit: () => {},
      });
      const snap = await api.getSessionSnapshot(session.id);
      conn.seedSnapshot(session.id, snap);
      conn.subscribe(session.id, { seq: snap.asOfSeq, epoch: snap.epoch });

      const submitted = await api.submitPrompt(session.id, {
        content: [{ type: 'text', text: 'Please delegate a coding task to a subagent' }],
        model: 'test/model',
      });
      conn.bindNextPromptId(session.id, submitted.promptId);

      // Wait for the session to go idle (main turn — including the subagent
      // run — has fully ended).
      await new Promise<void>((resolve, reject) => {
        const timeout = setTimeout(
          () => reject(new Error('timeout waiting for session to go idle')),
          90_000,
        );
        const check = (): void => {
          const idle = appEvents.some(
            (e) => e.type === 'sessionWorkChanged' && e.busy === false,
          );
          if (idle) {
            clearTimeout(timeout);
            resolve();
            return;
          }
          setTimeout(check, 250);
        };
        setTimeout(check, 250);
      });

      // The projector should have produced taskCreated for the subagent
      const taskCreatedEvents = appEvents.filter(
        (e) => e.type === 'taskCreated' && e.task.id !== session.id,
      );
      expect(taskCreatedEvents.length).toBeGreaterThanOrEqual(1);

      // Reduce to final task states and find the subagent task (not the main session)
      const tasks = reduceTaskEvents(appEvents);
      const subagentTask = Array.from(tasks.values()).find((t) => t.id !== session.id);
      expect(subagentTask).toBeDefined();
      expect(subagentTask?.status).toBe('completed');

      // The projector should have produced taskProgress with the subagent's text output
      const progressEvents = appEvents.filter(
        (e) => e.type === 'taskProgress' && e.taskId === subagentTask?.id,
      );
      expect(progressEvents.length).toBeGreaterThanOrEqual(1);

      // The output chunks should contain the subagent's response text
      const outputText = progressEvents
        .map((e) => (e.type === 'taskProgress' ? (e.outputChunk ?? '') : ''))
        .join('');
      expect(outputText).toContain('hello world');
      conn.unsubscribe(session.id);
    } finally {
      api.close();
    }
  }, 120_000);
});
