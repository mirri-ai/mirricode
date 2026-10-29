/**
 * Process orchestrator for journey scenarios (ADR-002, spec
 * server-e2e-architecture.md).
 *
 * Owns the lifecycle of REAL `mirri` processes: spawn with an isolated
 * MIRRICODE_HOME, wait for readiness, kill / graceful-stop, and one-shot
 * CLI subcommand execution (`mirri migrate` — journey stage ⑥). Journey
 * cases declare the process shape they need; this module is the only
 * place process birth/death happens.
 */
import { execFile, spawn, type ChildProcess } from 'node:child_process';
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';

const execFileAsync = promisify(execFile);

const REPO_ROOT = resolve(fileURLToPath(new URL('../../../', import.meta.url)));

/** Binary resolution: env override → local repo target → CARGO_TARGET_DIR (docker). */
function rustBin(): string {
  const envBin = process.env['MIRRICODE_E2E_RUST_BIN'];
  if (envBin !== undefined && envBin !== '') return envBin;
  const local = join(REPO_ROOT, 'rust', 'target', 'debug', 'mirri');
  const targetDir = process.env['CARGO_TARGET_DIR'];
  const container = targetDir !== undefined && targetDir !== '' ? join(targetDir, 'debug', 'mirri') : null;
  if (container !== null) return container;
  return local;
}

export interface ProcHandle {
  child: ChildProcess;
  port: number;
  baseUrl: string;
  homeDir: string;
  workDir: string;
}

export interface ServeOptions {
  /** Isolated home dir. Default: a fresh temp dir (the "new machine" state). */
  homeDir?: string;
  /** Workspace root served. Default: a fresh empty temp dir. */
  workDir?: string;
  /** Default: an auto-allocated free port. */
  port?: number;
  /** `bypass` (default) = --dangerous-bypass-auth; `file` = token persisted to <home>/server.token. */
  tokenMode?: 'bypass' | 'file';
  /** Written to <homeDir>/config.toml BEFORE spawn (use fakeProviderConfigToml). */
  configToml?: string;
}

/** Grab a free TCP port (bind 0, read, close). */
async function getFreePort(): Promise<number> {
  const { createServer } = await import('node:net');
  const netServer = createServer();
  await new Promise<void>((res, rej) => {
    netServer.once('error', rej);
    netServer.listen(0, '127.0.0.1', () => res());
  });
  const address = netServer.address();
  const port = typeof address === 'object' && address !== null ? address.port : 0;
  await new Promise<void>((res) => netServer.close(() => res()));
  return port;
}

/** Poll /api/v1/healthz until the envelope reports ok:true. */
async function waitHealthy(baseUrl: string, timeoutMs: number, onFail: () => string): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let lastError: unknown;
  while (Date.now() < deadline) {
    try {
      const res = await fetch(`${baseUrl}/api/v1/healthz`);
      if (res.ok) {
        const body = (await res.json()) as { data?: { ok?: boolean } };
        if (body.data?.ok === true) return;
      }
    } catch (error) {
      lastError = error;
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  const lastErrorText =
    lastError instanceof Error ? lastError.message : lastError !== undefined ? JSON.stringify(lastError) : 'unknown';
  throw new Error(`serve not healthy within ${timeoutMs}ms: ${lastErrorText}\n${onFail()}`);
}

/** Boot a real `mirri serve` process. Resolves when healthz reports ready. */
export async function startServe(opts: ServeOptions): Promise<ProcHandle> {
  const homeDir = opts.homeDir ?? mkdtempSync(join(tmpdir(), 'mirri-journey-home-'));
  const workDir = opts.workDir ?? mkdtempSync(join(tmpdir(), 'mirri-journey-work-'));
  const port = opts.port ?? (await getFreePort());

  mkdirSync(homeDir, { recursive: true });
  mkdirSync(workDir, { recursive: true });
  if (opts.configToml !== undefined) {
    writeFileSync(join(homeDir, 'config.toml'), opts.configToml, 'utf8');
  }

  const args = [
    'serve',
    '--host', '127.0.0.1',
    '--port', String(port),
    '--workspace', workDir,
    '--log-level', 'warn',
  ];
  if ((opts.tokenMode ?? 'bypass') === 'bypass') {
    args.push('--dangerous-bypass-auth');
  }

  const child = spawn(rustBin(), args, {
    stdio: ['ignore', 'pipe', 'pipe'],
    // `HOME` is remapped to the isolated home dir: the MCP global layer
    // resolves from `$HOME/.mirri-code/mcp.json`, so without the remap a
    // developer's real MCP servers leak into the "fresh machine" journey
    // environment (global constraint: zero sharing between cases).
    env: { ...process.env, MIRRICODE_HOME: homeDir, HOME: homeDir },
  });
  let stderrTail = '';
  child.stderr?.on('data', (chunk: Buffer) => {
    stderrTail = (stderrTail + chunk.toString()).slice(-4000);
  });

  const baseUrl = `http://127.0.0.1:${port}`;
  try {
    await waitHealthy(baseUrl, 30_000, () => stderrTail);
  } catch (error) {
    child.kill('SIGKILL');
    throw error;
  }
  return { child, port, baseUrl, homeDir, workDir };
}

/** Stop a serve process. `kill` = SIGKILL immediately (journey stage ⑤); `graceful` = POST /api/v1/shutdown. */
export async function stopServe(proc: ProcHandle, mode: 'graceful' | 'kill' = 'graceful'): Promise<void> {
  if (mode === 'kill') {
    proc.child.kill('SIGKILL');
    await waitExit(proc.child, 5000);
    return;
  }
  try {
    await fetch(`${proc.baseUrl}/api/v1/shutdown`, { method: 'POST', signal: AbortSignal.timeout(5000) });
  } catch {
    proc.child.kill('SIGKILL');
  }
  await waitExit(proc.child, 10_000);
  if (proc.child.exitCode === null) {
    proc.child.kill('SIGKILL');
    await waitExit(proc.child, 2000);
  }
}

async function waitExit(child: ChildProcess, timeoutMs: number): Promise<void> {
  await Promise.race([
    new Promise<void>((r) => child.once('exit', () => r())),
    new Promise<void>((r) => setTimeout(r, timeoutMs)),
  ]);
}

/** Run a one-shot `mirri <args>` subcommand (e.g. migrate) against a home dir; waits for exit. */
export async function runOneShot(args: string[], homeDir: string): Promise<{ code: number; stdout: string }> {
  try {
    const { stdout } = await execFileAsync(rustBin(), args, {
      env: { ...process.env, MIRRICODE_HOME: homeDir },
      timeout: 60_000,
      maxBuffer: 32 * 1024 * 1024,
    });
    return { code: 0, stdout };
  } catch (error) {
    const err = error as { code?: number; stdout?: string; stderr?: string };
    return { code: err.code ?? 1, stdout: err.stdout ?? '' + (err.stderr ?? '') };
  }
}

/** A valid config.toml pointing default_model at the fake provider (top-level default_model — engine requirement). */
export function fakeProviderConfigToml(fakeUrl: string): string {
  return `default_model = "fake/model"

[providers.fake]
type = "openai"
api_key = "sk-fake"
base_url = "${fakeUrl}"

[models."fake/model"]
provider = "fake"
model = "fake-model"
max_context_size = 128000
`;
}
