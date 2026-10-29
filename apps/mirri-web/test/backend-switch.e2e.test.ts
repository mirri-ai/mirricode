// apps/mirri-web/test/backend-switch.e2e.test.ts
// End-to-end coverage of the Vite dev-server backend switcher: the dev proxy
// defaults to the Rust core engine — at boot (MIRRICODE_BACKEND) and through
// the dev-only `POST /__mirri-dev/backend` endpoint, which is what the Sidebar
// backend-pill menu calls.
//
// Reachability is proven the same way the plan states it: the `/api/v1/meta`
// payload observed THROUGH the Vite proxy must carry the backend marker AND the
// exact serverId of the engine started directly in this test (so a mismatch
// cannot be a coincidence of two engines answering).
//
// Set MIRRICODE_E2E_SKIP_RUST=1 to skip the whole suite (escape hatch for
// environments without the prebuilt `rust/target/debug/mirri` binary).

import { execFile, spawn, type ChildProcess } from 'node:child_process';
import { existsSync } from 'node:fs';
import { promisify } from 'node:util';
import { mkdtemp, rm } from 'node:fs/promises';
import { createServer as createNetServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { createServer as createViteServer, type ViteDevServer } from 'vite';
import WebSocket from 'ws';

// ---------------------------------------------------------------------------
// Paths / constants
// ---------------------------------------------------------------------------

const REPO_ROOT = resolve(fileURLToPath(new URL('../../../', import.meta.url)));
const RUST_BIN = join(REPO_ROOT, 'rust', 'target', 'debug', 'mirri');
const VITE_CONFIG = join(REPO_ROOT, 'apps', 'mirri-web', 'vite.config.ts');
const WEB_ROOT = join(REPO_ROOT, 'apps', 'mirri-web');

/** `execFile` without a callback returns a ChildProcess, not a Promise — promisify it so `await` actually waits for the build. */
const execFileAsync = promisify(execFile);

const BEARING_ENV_KEYS = [
  'MIRRICODE_BACKEND_RUST_URL',
  'MIRRICODE_BACKEND_V3_URL',
  'MIRRICODE_BACKEND',
  'MIRRICODE_SERVER_URL',
] as const;

const skipRust = process.env.MIRRICODE_E2E_SKIP_RUST === '1';

interface MetaInfo {
  backend: string;
  serverId: string;
}

/** GET /api/v1/meta, unwrapping the `{code,msg,data}` envelope when present. */
async function fetchMeta(base: string): Promise<MetaInfo> {
  const res = await fetch(`${base}/api/v1/meta`);
  if (!res.ok) throw new Error(`GET /api/v1/meta -> HTTP ${res.status}`);
  const body = (await res.json()) as {
    backend?: string;
    data?: {
      backend?: string;
      serverId?: string;
      server_id?: string;
    };
  };
  const data = body.data ?? body;
  const backend = data.backend ?? body.backend ?? '';
  const serverId = data.serverId ?? data.server_id ?? '';
  if (!backend || !serverId) throw new Error(`unexpected /api/v1/meta payload: ${JSON.stringify(body)}`);
  return { backend, serverId };
}

async function getFreePort(): Promise<number> {
  return new Promise((resolvePort, reject) => {
    const srv = createNetServer();
    srv.on('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const addr = srv.address();
      const port = typeof addr === 'object' && addr !== null ? addr.port : undefined;
      srv.close(() => (port === undefined ? reject(new Error('no free port')) : resolvePort(port)));
    });
  });
}

/** Poll /api/v1/healthz until the envelope reports ok:true. */
async function waitForHealthz(port: number, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let lastError: unknown = undefined;
  while (Date.now() < deadline) {
    try {
      const res = await fetch(`http://127.0.0.1:${port}/api/v1/healthz`);
      if (res.ok) {
        const body = (await res.json()) as { data?: { ok?: boolean } };
        if (body.data?.ok === true) return;
      }
    } catch (error) {
      lastError = error;
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(
    `server on :${port} not ready within ${timeoutMs}ms` +
      (lastError !== undefined
        ? ` (last error: ${lastError instanceof Error ? lastError.message : JSON.stringify(lastError)})`
        : ''),
  );
}

/**
 * Spawn the real rust engine binary against a temp home. If the binary does
 * not exist yet, build it first (debug profile — this suite is a dev-loop
 * gate, not a release gate).
 */
async function spawnRustServer(port: number): Promise<{ child: ChildProcess; dir: string }> {
  const dir = await mkdtemp(join(tmpdir(), 'mirri-web-e2e-rust-'));
  if (!existsSync(RUST_BIN)) {
    await execFileAsync('cargo', [
      'build',
      '-p',
      'mirri-cli',
      '--manifest-path',
      join(REPO_ROOT, 'rust', 'Cargo.toml'),
    ]);
  }
  const child = spawn(
    RUST_BIN,
    [
      'serve',
      '--host',
      '127.0.0.1',
      '--port',
      String(port),
      '--workspace',
      dir,
      '--log-level',
      'warn',
      // Loopback open mode, so the credential-less dev proxy can talk to the
      // engine. The rust server otherwise boots with the persistent bearer
      // token by default.
      '--dangerous-bypass-auth',
    ],
    {
      cwd: REPO_ROOT,
      stdio: ['ignore', 'ignore', 'pipe'],
      // Isolate the engine's Mirri home inside the throwaway workspace dir:
      // without this the session-index write-through registers the temp
      // workspace in the real `~/.mirri-code/workspaces.json`.
      env: { ...process.env, MIRRICODE_HOME: join(dir, 'mirri-home') },
    },
  );
  let stderr = '';
  child.stderr?.on('data', (chunk: Buffer) => {
    stderr = (stderr + chunk.toString()).slice(-4000);
  });
  try {
    await waitForHealthz(port, 30_000);
  } catch (error) {
    child.kill('SIGKILL');
    await rm(dir, { recursive: true, force: true });
    throw new Error(`rust engine failed to boot: ${stderr}\n${String(error)}`, { cause: error });
  }
  return { child, dir };
}

/** Politely stop the rust engine via /api/v1/shutdown, falling back to SIGKILL. */
async function stopRustServer(proc: { child: ChildProcess; dir: string }, base: string): Promise<void> {
  try {
    await fetch(`${base}/api/v1/shutdown`, { method: 'POST' });
    await Promise.race([
      new Promise<void>((r) => proc.child.once('exit', () => r())),
      new Promise<void>((r) => setTimeout(r, 5000)),
    ]);
  } catch {
    // not running — fine
  }
  if (proc.child.exitCode === null) proc.child.kill('SIGKILL');
  await rm(proc.dir, { recursive: true, force: true });
}

interface DevServerHandle {
  server: ViteDevServer;
  port: number;
}

let savedEnv: Partial<Record<(typeof BEARING_ENV_KEYS)[number], string | undefined>> = {};

function saveEnv(): void {
  for (const key of BEARING_ENV_KEYS) savedEnv[key] = process.env[key];
}

function restoreEnv(): void {
  for (const key of BEARING_ENV_KEYS) {
    const value = savedEnv[key];
    if (value === undefined) delete process.env[key];
    else process.env[key] = value;
  }
}

/**
 * Boot a Vite dev server whose config sees exactly the given backend env.
 * Vite loads `vite.config.ts` during createServer, so setting process.env
 * beforehand is what decides the initial proxy target.
 */
async function startViteDevServer(opts: {
  rustUrl: string;
  backend?: string;
}): Promise<DevServerHandle> {
  const port = await getFreePort();
  process.env.MIRRICODE_BACKEND_RUST_URL = opts.rustUrl;
  process.env.MIRRICODE_BACKEND_V3_URL = opts.rustUrl;
  delete process.env.MIRRICODE_SERVER_URL;
  if (opts.backend === undefined) delete process.env.MIRRICODE_BACKEND;
  else process.env.MIRRICODE_BACKEND = opts.backend;

  const server = await createViteServer({
    configFile: VITE_CONFIG,
    root: WEB_ROOT,
    logLevel: 'silent',
    server: { host: '127.0.0.1', port, strictPort: true },
  });
  await server.listen();
  return { server, port };
}

/** First server frame of the JSON text-frame protocol `/api/v1/ws`. */
function readFirstWsFrame(url: string, timeoutMs: number): Promise<{ type: string; serverId?: string }> {
  return new Promise((resolveFrame, reject) => {
    const ws = new WebSocket(url);
    const timer = setTimeout(() => {
      ws.terminate();
      reject(new Error(`websocket handshake timed out after ${timeoutMs}ms: ${url}`));
    }, timeoutMs);
    ws.on('message', (frame) => {
      try {
        const parsed = JSON.parse(frame.toString()) as { type?: string; serverId?: string };
        if (parsed.type === 'server_hello') {
          clearTimeout(timer);
          ws.close();
          resolveFrame(parsed);
        }
      } catch {
        // non-JSON frame — keep waiting for server_hello
      }
    });
    ws.on('error', (err) => {
      clearTimeout(timer);
      reject(err);
    });
    ws.on('close', () => {
      // Server closed before sending server_hello — don't hang the full timeout.
      // If the socket closed after we resolved (we call ws.close() on success),
      // the Promise is already settled and this reject is a no-op.
      clearTimeout(timer);
      reject(new Error('websocket closed before server_hello'));
    });
  });
}

// ---------------------------------------------------------------------------
// Suite
// ---------------------------------------------------------------------------

describe.skipIf(skipRust)('mirri-web dev backend switching (rust default)', () => {
  let rust: { child: ChildProcess; dir: string } | undefined;
  let rustBase = '';
  let rustServerId = '';
  const liveDevServers: DevServerHandle[] = [];

  beforeAll(async () => {
    saveEnv();

    const port = await getFreePort();
    rust = await spawnRustServer(port);
    rustBase = `http://127.0.0.1:${port}`;
    rustServerId = (await fetchMeta(rustBase)).serverId;
  }, 120_000);

  afterAll(async () => {
    for (const dev of liveDevServers.splice(0)) {
      await dev.server.close();
    }
    if (rust !== undefined) {
      await stopRustServer(rust, rustBase);
      rust = undefined;
    }
    restoreEnv();
  }, 30_000);

  afterEach(async () => {
    for (const dev of liveDevServers.splice(0)) {
      await dev.server.close();
    }
  });

  it('should proxy /api/v1 to the rust engine by default when MIRRICODE_BACKEND is unset', async () => {
    const dev = await startViteDevServer({ rustUrl: rustBase });
    liveDevServers.push(dev);
    const origin = `http://127.0.0.1:${dev.port}`;

    const meta = await fetchMeta(origin);
    expect(meta.backend).toBe('v3');
    expect(meta.serverId).toBe(rustServerId);

    const switcher = (await (await fetch(`${origin}/__mirri-dev/backend`)).json()) as {
      current: string;
    };
    expect(switcher.current).toBe(rustBase);
  }, 30_000);

  it('should reject switching to an unknown backend name', async () => {
    const dev = await startViteDevServer({ rustUrl: rustBase });
    liveDevServers.push(dev);
    const origin = `http://127.0.0.1:${dev.port}`;

    const res = await fetch(`${origin}/__mirri-dev/backend`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ name: 'nope' }),
    });
    expect(res.status).toBe(400);

    // The failed switch must not have moved the proxy.
    const meta = await fetchMeta(origin);
    expect(meta.backend).toBe('v3');
  }, 30_000);

  it('should complete a websocket handshake with the rust engine through the dev proxy', async () => {
    const dev = await startViteDevServer({ rustUrl: rustBase });
    liveDevServers.push(dev);
    const origin = `http://127.0.0.1:${dev.port}`;

    const hello = await readFirstWsFrame(`${origin.replace(/^http/, 'ws')}/api/v1/ws`, 10_000);
    expect(hello.type).toBe('server_hello');
    expect(hello.serverId).toBe(rustServerId);
  }, 30_000);
});
