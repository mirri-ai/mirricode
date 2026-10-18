/**
 * Rust-backed `mirri acp` — spawn the `mirri-acp` binary as a child process
 * when `MIRRICODE_RUST_ENGINE` is truthy.
 *
 * Unlike the web backend (which runs an HTTP server and polls `/healthz`),
 * ACP communicates over **stdio** (NDJSON on stdin/stdout). The child's
 * stdin/stdout must be inherited (passthrough) so the ACP client talks
 * directly to the Rust binary. Only stderr is captured for log forwarding.
 *
 * The child exits when the client closes stdin (its `serve` loop breaks on
 * `TransportError::Closed`), so this function resolves on child exit.
 */

import { spawn, spawnSync, type ChildProcess } from 'node:child_process';
import { existsSync } from 'node:fs';
import { join } from 'node:path';

import { CLI_SHUTDOWN_TIMEOUT_MS } from '#/constant/app';
import { getHostPackageRoot } from '#/cli/version';

/** Env override for a prebuilt `mirri-acp` binary path. */
const RUST_ACP_BIN_ENV = 'MIRRICODE_RUST_ACP_BIN';

const MIRRI_ACP_BIN_NAME =
  process.platform === 'win32' ? 'mirri-acp.exe' : 'mirri-acp';

/**
 * Start the Rust `mirri-acp` binary as a child process with stdio passthrough.
 *
 * The function blocks until the child exits. It forwards `MIRRICODE_HOME` and
 * `MIRRI_ACP_WORKSPACE_DIR` into the child environment, and sets `RUST_LOG`
 * from the existing env or defaults to `info`.
 */
export async function runRustAcpServer(): Promise<void> {
  const rustWorkspaceDir = join(getHostPackageRoot(), '..', '..', 'rust');
  const binaryPath =
    resolveMirriAcpBinary(rustWorkspaceDir) ??
    buildMirriAcpBinary(rustWorkspaceDir);

  const childEnv: NodeJS.ProcessEnv = {
    ...process.env,
    MIRRI_ACP_WORKSPACE_DIR: process.cwd(),
  };
  childEnv['RUST_LOG'] = process.env['RUST_LOG'] ?? 'info';

  // stdio: inherit stdin/stdout (ACP protocol channel), pipe stderr for logs.
  const child = spawn(binaryPath, [], {
    stdio: ['inherit', 'inherit', 'pipe'],
    env: childEnv,
  });

  // Forward stderr to the parent's stderr (Rust tracing logs go there).
  child.stderr?.on('data', (chunk: Buffer) => {
    process.stderr.write(chunk);
  });

  // Handle spawn errors.
  child.on('error', (error) => {
    process.stderr.write(`mirri-acp: failed to start: ${error.message}\n`);
    process.exit(1);
  });

  // Forward signals.
  const onSignal = (signal: NodeJS.Signals): void => {
    if (child.exitCode === null) {
      child.kill(signal);
    }
  };
  process.on('SIGINT', onSignal);
  process.on('SIGTERM', onSignal);

  // Wait for the child to exit.
  const exitCode = await new Promise<number | null>((resolve) => {
    child.once('exit', resolve);
  });

  process.removeListener('SIGINT', onSignal);
  process.removeListener('SIGTERM', onSignal);

  process.exit(exitCode ?? 0);
}

/**
 * Locate a prebuilt `mirri-acp` binary, or `undefined` when none exists.
 * An explicit `MIRRICODE_RUST_ACP_BIN` path wins; otherwise the release
 * profile is preferred over debug inside `rust/target/`.
 */
function resolveMirriAcpBinary(
  rustWorkspaceDir: string,
  env: NodeJS.ProcessEnv = process.env,
): string | undefined {
  const overridePath = env[RUST_ACP_BIN_ENV];
  if (overridePath !== undefined && overridePath !== '') return overridePath;

  for (const profile of ['release', 'debug'] as const) {
    const candidate = join(rustWorkspaceDir, 'target', profile, MIRRI_ACP_BIN_NAME);
    if (existsSync(candidate)) return candidate;
  }
  return undefined;
}

/**
 * Build the `mirri-acp` binary with `cargo build --bin mirri-acp` (debug
 * profile) and return its path. Throws when the workspace is absent or the
 * build fails.
 */
function buildMirriAcpBinary(rustWorkspaceDir: string): string {
  if (!existsSync(join(rustWorkspaceDir, 'Cargo.toml'))) {
    throw new Error(
      `Rust workspace not found at ${rustWorkspaceDir}; MIRRICODE_RUST_ENGINE requires the mirricode checkout, or set ${RUST_ACP_BIN_ENV} to a prebuilt mirri-acp binary`,
    );
  }
  process.stderr.write(
    'no prebuilt mirri-acp binary found; running `cargo build --bin mirri-acp` (debug profile)\n',
  );
  const result = spawnSync('cargo', ['build', '--bin', 'mirri-acp'], {
    cwd: rustWorkspaceDir,
    stdio: 'inherit',
  });
  if (result.error !== undefined) {
    throw new Error(`failed to run cargo build --bin mirri-acp: ${result.error.message}`, {
      cause: result.error,
    });
  }
  if (result.status !== 0) {
    throw new Error(`cargo build --bin mirri-acp failed with exit code ${String(result.status)}`);
  }
  const builtPath = join(rustWorkspaceDir, 'target', 'debug', MIRRI_ACP_BIN_NAME);
  process.stderr.write(`Rust ACP binary built: ${builtPath}\n`);
  return builtPath;
}

/**
 * Gracefully stop a child process: SIGINT first, then SIGKILL after timeout.
 */
export async function stopChildProcess(
  child: ChildProcess,
  timeoutMs: number = CLI_SHUTDOWN_TIMEOUT_MS,
): Promise<void> {
  if (child.exitCode !== null) return;
  if (!child.kill('SIGINT')) return;
  await Promise.race([
    new Promise<void>((resolve) => child.once('exit', () => resolve())),
    new Promise<void>((resolve) => setTimeout(resolve, timeoutMs)),
  ]);
  if (child.exitCode === null) {
    child.kill('SIGKILL');
  }
}
