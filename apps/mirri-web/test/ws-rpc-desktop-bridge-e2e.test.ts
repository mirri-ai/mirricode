// apps/mirri-web/test/ws-rpc-desktop-bridge-e2e.test.ts
//
// The tier-1 desktop-bridge chain against REAL engine state (P11a F-P9-8):
// the rust engine (the prebuilt `mirri serve` binary) serves a real session
// whose view echoes the workspace root as `cwd`, and the "open in editor"
// tier must resolve workspace-relative paths against THAT engine-provided
// cwd — not a hand-mocked one (the unit suite's tier-1 tests mock
// `session.get`, which is exactly the false confidence F-P9-8 called out).
//
// Only the Electron preload bridge itself is a test double — the engine side
// of the chain (session view, cwd echo, JSON-RPC transport) runs for real.
//
// The spawned engine runs with `MIRRICODE_HOME` pointed inside the throwaway
// tempdir: without it the engine resolves the real `~/.mirri-code` and its
// session-index write-through registers the temp workspace in the user's
// `workspaces.json` (the rust ACP conformance suite isolates the same way).
//
// Set MIRRICODE_E2E_SKIP_RUST=1 to skip the suite (escape hatch for
// environments without the prebuilt `rust/target/debug/mirri` binary).

import { execFile, spawn, type ChildProcess } from 'node:child_process';
import { existsSync } from 'node:fs';
import { promisify } from 'node:util';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { createServer as createNetServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';

import { WsRpcApi, buildRpcWsUrl } from '../src/api/daemon/ws-rpc-api';
import type { MirriDesktopBridge } from '../src/lib/desktopBridge';

const REPO_ROOT = resolve(fileURLToPath(new URL('../../../', import.meta.url)));
const RUST_BIN = join(REPO_ROOT, 'rust', 'target', 'debug', 'mirri');

const execFileAsync = promisify(execFile);

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

/** A desktop-bridge double: every method records its call and succeeds. */
function makeBridge(): MirriDesktopBridge & {
  openInEditor: ReturnType<typeof vi.fn>;
  revealPath: ReturnType<typeof vi.fn>;
} {
  return {
    openPath: vi.fn().mockResolvedValue({ opened: true }),
    revealPath: vi.fn().mockResolvedValue({ revealed: true }),
    openInEditor: vi.fn().mockResolvedValue({ opened: true }),
    openInApp: vi.fn().mockResolvedValue({ opened: true }),
    listOpenInApps: vi.fn().mockResolvedValue([]),
    openExternal: vi.fn().mockResolvedValue({ opened: true }),
    notify: vi.fn().mockResolvedValue({ shown: true }),
    setBadgeCount: vi.fn().mockResolvedValue({ set: true }),
    dockBounce: vi.fn().mockResolvedValue(undefined),
    setLoginItem: vi.fn().mockResolvedValue({ open: true }),
    getLoginItem: vi.fn().mockResolvedValue({ open: false }),
    onDeepLink: vi.fn(),
    onNotificationClick: vi.fn(),
  } as unknown as MirriDesktopBridge & {
    openInEditor: ReturnType<typeof vi.fn>;
    revealPath: ReturnType<typeof vi.fn>;
  };
}

describe.skipIf(skipRust)('WsRpcApi tier-1 desktop bridge over a real engine session', () => {
  let rust: { child: ChildProcess; dir: string } | undefined;
  let base = '';

  beforeAll(async () => {
    const dir = await mkdtemp(join(tmpdir(), 'mirri-web-bridge-e2e-'));
    if (!existsSync(RUST_BIN)) {
      await execFileAsync('cargo', ['build', '-p', 'mirri-cli'], {
        cwd: join(REPO_ROOT, 'rust'),
        timeout: 600_000,
        maxBuffer: 32 * 1024 * 1024,
      });
    }
    const port = await getFreePort();
    // Seed the engine's default_model (spec 11 R4 — the T1 fail-loud kernel
    // rejects a session.create without a resolvable model, and this suite's
    // `createSession({})` sends no modelId): a throwaway provider + alias,
    // the same shape the rust-side fixtures seed (4cf03e13). No
    // max_context_size on purpose — the context window stays unknown, so no
    // compaction-aware behavior changes under the suite.
    const homeDir = join(dir, 'mirri-home');
    await mkdir(homeDir, { recursive: true });
    await writeFile(
      join(homeDir, 'config.toml'),
      [
        'default_model = "test-model"',
        '',
        '[providers.test]',
        'type = "openai"',
        'api_key = "sk-test"',
        'base_url = "http://127.0.0.1:1"',
        '',
        '[models.test-model]',
        'provider = "test"',
        'model = "gpt-test"',
        // Required by the TOML schema (the CLI config loader rejects an
        // alias without a window); the bridge suite never prompts, so the
        // value only needs to parse.
        'max_context_size = 128000',
        '',
      ].join('\n'),
      'utf8',
    );
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
        '--dangerous-bypass-auth',
      ],
      {
        stdio: ['ignore', 'pipe', 'pipe'],
        // Isolate the engine's Mirri home inside the throwaway workspace dir:
        // without this the session-index write-through registers the temp
        // workspace in the real `~/.mirri-code/workspaces.json`.
        env: { ...process.env, MIRRICODE_HOME: homeDir },
      },
    );
    rust = { child, dir };
    await waitForHealthz(port, 30_000);
    base = `http://127.0.0.1:${port}`;
  }, 120_000);

  afterAll(async () => {
    if (rust !== undefined) {
      try {
        await fetch(`${base}/api/v1/shutdown`, { method: 'POST' });
        await Promise.race([
          new Promise<void>((r) => rust!.child.once('exit', () => r())),
          new Promise<void>((r) => setTimeout(r, 5000)),
        ]);
      } catch {
        // not running — fine
      }
      if (rust.child.exitCode === null) rust.child.kill('SIGKILL');
      await rm(rust.dir, { recursive: true, force: true });
      rust = undefined;
    }
  }, 30_000);

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('should resolve openFile against the engine-echoed cwd when the bridge is present', async () => {
    expect(rust).toBeDefined();
    // The engine keeps the workspace root LEXICALLY resolved (W9: serve
    // no longer canonicalizes — a symlinked root must register as the
    // spelling it was given), so the echoed cwd is the tempdir as spawned.
    const workspaceRoot = rust!.dir;

    // A workspace file the editor open targets.
    await writeFile(join(workspaceRoot, 'README.md'), '# bridge e2e\n', 'utf8');

    // Given the real engine serving a real session (no cwd mocked — the
    // session view's cwd comes from the engine's workspace root echo)
    const api = new WsRpcApi(buildRpcWsUrl(base), base, undefined);
    await api.connect();
    const session = await api.createSession({});
    expect(session.cwd).toBe(workspaceRoot);

    // And a desktop bridge present (the Electron preload double — the
    // engine side of the chain is real)
    const bridge = makeBridge();
    vi.stubGlobal('window', { mirriDesktop: bridge });

    // When a workspace-relative file is opened through the tier-1 chain
    // (no explicit cwd hint — the engine's session view is the source)
    await expect(
      api.openFile(session.id, { path: 'README.md', line: 7 }),
    ).resolves.toEqual({ opened: true });

    // Then the bridge received the path resolved against the ENGINE's
    // workspace root — the end-to-end link F-P9-8 restored.
    expect(bridge.openInEditor).toHaveBeenCalledWith(
      join(workspaceRoot, 'README.md'),
      7,
    );

    // And revealFile resolves through the same engine-provided root
    await expect(api.revealFile(session.id, { path: 'README.md' })).resolves.toEqual({
      revealed: true,
    });
    expect(bridge.revealPath).toHaveBeenCalledWith(join(workspaceRoot, 'README.md'));

    api.close();
  });
});
