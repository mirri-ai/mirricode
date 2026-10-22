import { execFile } from 'node:child_process';
import { userInfo } from 'node:os';

/** Timeout for the login-shell environment probe. */
const SHELL_ENV_TIMEOUT_MS = 5_000;

/**
 * Resolve the user's full shell environment by running their login shell in
 * interactive mode (`$SHELL -lic /usr/bin/env`). GUI-launched apps inherit a
 * minimal `PATH` that misses nvm, Homebrew, and other user-installed tool
 * directories. The login-shell profile alone (`-l`) is not enough because many
 * PATH additions live in `.bashrc` / `.zshrc` which are only sourced for
 * interactive shells. The `-i` flag forces bash/zsh into interactive mode so
 * those files are read.
 *
 * Returns the merged environment, or `undefined` when the probe fails (no
 * shell, spawn error, timeout). The caller falls back to `process.env` in that
 * case.
 */
export function resolveShellEnv(): Promise<Record<string, string> | undefined> {
  const envShell = process.env['SHELL']?.trim();
  let shell: string | undefined;
  if (envShell !== undefined && envShell.length > 0) {
    shell = envShell;
  } else {
    try {
      const s = userInfo().shell;
      shell = s !== null && s.length > 0 ? s : undefined;
    } catch {
      shell = undefined;
    }
  }

  if (shell === undefined) return tryZshShellEnv();

  return new Promise((resolve) => {
    execFile(
      shell,
      ['-lic', '/usr/bin/env'],
      { encoding: 'utf8', timeout: SHELL_ENV_TIMEOUT_MS, windowsHide: true },
      (error, stdout) => {
        if (error !== null) {
          // Fall back to zsh — many users have $SHELL=bash but their PATH
          // config lives in .zshrc (common when using Warp terminal).
          void tryZshShellEnv().then(resolve);
          return;
        }
        const env = parseEnvOutput(stdout);
        if (env === undefined) {
          void tryZshShellEnv().then(resolve);
          return;
        }
        resolve(env);
      },
    );
  });
}

/**
 * Fallback: try zsh when the primary shell probe failed or returned an
 * incomplete PATH.
 */
function tryZshShellEnv(): Promise<Record<string, string> | undefined> {
  return new Promise((resolve) => {
    execFile(
      '/bin/zsh',
      ['-lic', '/usr/bin/env'],
      { encoding: 'utf8', timeout: SHELL_ENV_TIMEOUT_MS, windowsHide: true },
      (error, stdout) => {
        if (error !== null) {
          resolve(undefined);
          return;
        }
        resolve(parseEnvOutput(stdout));
      },
    );
  });
}

function parseEnvOutput(stdout: string): Record<string, string> | undefined {
  const env: Record<string, string> = {};
  for (const line of stdout.split('\n')) {
    const eq = line.indexOf('=');
    if (eq <= 0) continue;
    const key = line.slice(0, eq);
    const value = line.slice(eq + 1).trim();
    if (key.length > 0 && value.length > 0) env[key] = value;
  }
  if (env['PATH'] === undefined || env['PATH'].length === 0) {
    return undefined;
  }
  return env;
}
