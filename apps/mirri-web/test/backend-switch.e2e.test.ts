// apps/mirri-web/test/backend-switch.e2e.test.ts
// End-to-end coverage of the Vite dev-server backend switcher: the dev proxy
// defaults to the Rust core engine and can be repointed to the kap-server (v2)
// engine and back — both at boot (MIRRICODE_BACKEND) and at runtime (the
// dev-only `POST /__mirri-dev/backend` endpoint, which is what the Sidebar
// backend-pill menu calls).
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
import {
  startServer,
  type RunningServer,
  type ServerHostIdentity,
} from '@mirri-ai/kap-server';

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
  'MIRRICODE_BACKEND_V2_URL',
  'MIRRICODE_BACKEND_V1_URL',
  'MIRRICODE_BACKEND',
  'MIRRICODE_SERVER_URL',
] as const;

const TEST_HOST_IDENTITY: ServerHostIdentity = {
  productName: 'mirri-web-e2e',
  version: '0.0.0-e2e',
  platform: 'test_platform',
};

const skipRust = process.env.MIRRICODE_E2E_SKIP_RUST === '1';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Grab a free TCP port on loopback (close the placeholder before returning). */
function getFreePort(): Promise<number> {
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

/** Spawn the prebuilt rust binary (building it first if the plan's binary is missing). */
async function spawnRustServer(port: number): Promise<{ child: ChildProcess; dir: string }> {
  const dir = await mkdtemp(join(tmpdir(), 'mirri-web-e2e-rust-'));
  if (!existsSync(RUST_BIN)) {
    await execFileAsync('cargo', ['build', '-p', 'mirri-cli'], {
      cwd: join(REPO_ROOT, 'rust'),
      timeout: 600_000,
      maxBuffer: 32 * 1024 * 1024,
    });
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
      // Loopback open mode — matches how the v2 engine runs in this suite
      // (disableAuth: true), so the credential-less dev proxy can talk to
      // both engines interchangeably. The rust server otherwise boots with
      // the persistent bearer token by default.
      '--dangerous-bypass-auth',
    ],
    {
      stdio: ['ignore', 'pipe', 'pipe'],
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

/** Start the v2 engine in-process (mirri-web tests may import kap-server). */
async function startKapServer(): Promise<{ server: RunningServer; base: string; dir: string }> {
  const dir = await mkdtemp(join(tmpdir(), 'mirri-web-e2e-v2-'));
  const server = await startServer({
    hostIdentity: TEST_HOST_IDENTITY,
    host: '127.0.0.1',
    port: 0,
    homeDir: dir,
    logLevel: 'silent',
    // Loopback open mode — matches how the rust engine runs in this suite, so
    // the dev proxy (which strips Origin but carries no credential) can talk
    // to both engines interchangeably.
    disableAuth: true,
  });
  await server.serverReadyPromise;
  return { server, base: `http://127.0.0.1:${server.port}`, dir };
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
  v2Url: string;
  backend?: string;
}): Promise<DevServerHandle> {
  const port = await getFreePort();
  process.env.MIRRICODE_BACKEND_RUST_URL = opts.rustUrl;
  process.env.MIRRICODE_BACKEND_V2_URL = opts.v2Url;
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
  let kap: Awaited<ReturnType<typeof startKapServer>> | undefined;
  let rustServerId = '';
  let kapServerId = '';
  const liveDevServers: DevServerHandle[] = [];

  beforeAll(async () => {
    saveEnv();
    kap = await startKapServer();
    kapServerId = (await fetchMeta(kap.base)).serverId;

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
    if (kap !== undefined) {
      await kap.server.close();
      await rm(kap.dir, { recursive: true, force: true });
      kap = undefined;
    }
    restoreEnv();
  }, 30_000);

  afterEach(async () => {
    for (const dev of liveDevServers.splice(0)) {
      await dev.server.close();
    }
  });

  it('should proxy /api/v1 to the rust engine by default when MIRRICODE_BACKEND is unset', async () => {
    const dev = await startViteDevServer({ rustUrl: rustBase, v2Url: kap!.base });
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

  it('should route /api/v1 to the v2 engine after switching from rust', async () => {
    const dev = await startViteDevServer({ rustUrl: rustBase, v2Url: kap!.base });
    liveDevServers.push(dev);
    const origin = `http://127.0.0.1:${dev.port}`;

    const switchRes = await fetch(`${origin}/__mirri-dev/backend`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ name: 'v2' }),
    });
    expect(switchRes.ok).toBe(true);

    const meta = await fetchMeta(origin);
    expect(meta.backend).toBe('v2');
    expect(meta.serverId).toBe(kapServerId);

    const switcher = (await (await fetch(`${origin}/__mirri-dev/backend`)).json()) as {
      current: string;
    };
    expect(switcher.current).toBe(kap!.base);
  }, 30_000);

  it('should route /api/v1 back to the rust engine after switching from v2', async () => {
    const dev = await startViteDevServer({ rustUrl: rustBase, v2Url: kap!.base });
    liveDevServers.push(dev);
    const origin = `http://127.0.0.1:${dev.port}`;

    for (const name of ['v2', 'v3']) {
      const res = await fetch(`${origin}/__mirri-dev/backend`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ name }),
      });
      expect(res.ok).toBe(true);
    }

    const meta = await fetchMeta(origin);
    expect(meta.backend).toBe('v3');
    expect(meta.serverId).toBe(rustServerId);
  }, 30_000);

  it('should boot with the v2 engine when MIRRICODE_BACKEND=v2', async () => {
    const dev = await startViteDevServer({ rustUrl: rustBase, v2Url: kap!.base, backend: 'v2' });
    liveDevServers.push(dev);
    const origin = `http://127.0.0.1:${dev.port}`;

    const meta = await fetchMeta(origin);
    expect(meta.backend).toBe('v2');
    expect(meta.serverId).toBe(kapServerId);

    const switcher = (await (await fetch(`${origin}/__mirri-dev/backend`)).json()) as {
      current: string;
    };
    expect(switcher.current).toBe(kap!.base);
  }, 30_000);

  it('should reject switching to an unknown backend name', async () => {
    const dev = await startViteDevServer({ rustUrl: rustBase, v2Url: kap!.base });
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
    const dev = await startViteDevServer({ rustUrl: rustBase, v2Url: kap!.base });
    liveDevServers.push(dev);
    const origin = `http://127.0.0.1:${dev.port}`;

    const hello = await readFirstWsFrame(`${origin.replace(/^http/, 'ws')}/api/v1/ws`, 10_000);
    expect(hello.type).toBe('server_hello');
    expect(hello.serverId).toBe(rustServerId);
  }, 30_000);
});