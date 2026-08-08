/**
 * `mirri web` — handleWebCommand unit tests.
 *
 * These tests don't start a real server: the foreground runner is injected,
 * and the tests drive its ready hook to verify the banner, token handling,
 * and the browser-open behavior (--no-open / bypass-auth).
 */

import { describe, expect, it, vi } from 'vitest';

import type { StartForegroundHooks, WebCommandDeps } from '#/cli/sub/web/run';
import { handleWebCommand } from '#/cli/sub/web/run';

function stripAnsi(text: string): string {
  return text.replaceAll(/\u001B\[[0-9;]*m/g, '');
}

function makeDeps(overrides: Partial<WebCommandDeps> = {}): WebCommandDeps & {
  writes: string[];
  openUrl: ReturnType<typeof vi.fn>;
} {
  const writes: string[] = [];
  const openUrl = vi.fn();
  return {
    startServerForeground: async (
      _options: unknown,
      hooks?: StartForegroundHooks,
    ): Promise<never> => {
      hooks?.onReady?.('http://127.0.0.1:58627');
      return undefined as unknown as never;
    },
    resolveToken: () => 'tok-1',
    stdout: {
      write(chunk: string | Uint8Array) {
        writes.push(String(chunk));
        return true;
      },
    },
    stderr: { write: () => true },
    ...overrides,
    writes,
    openUrl,
  };
}

describe('handleWebCommand (`mirri web`)', () => {
  it('prints the ready banner and opens the browser with the token fragment', async () => {
    const deps = makeDeps();
    await handleWebCommand({ port: '58627', open: true }, deps);

    const plain = stripAnsi(deps.writes.join(''));
    expect(plain).toContain('Mirri server');
    expect(plain).toContain('http://127.0.0.1:58627/');
    expect(plain).toContain('Token:    tok-1');
    expect(deps.openUrl).toHaveBeenCalledWith('http://127.0.0.1:58627/#token=tok-1');
  });

  it('skips the browser when --no-open is passed', async () => {
    const deps = makeDeps();
    await handleWebCommand({ port: '58627', open: false }, deps);

    expect(deps.openUrl).not.toHaveBeenCalled();
  });

  it('opens the plain origin when no token is available', async () => {
    const deps = makeDeps({ resolveToken: () => undefined });
    await handleWebCommand({ port: '58627', open: true }, deps);

    const plain = stripAnsi(deps.writes.join(''));
    expect(plain).not.toContain('Token:');
    expect(deps.openUrl).toHaveBeenCalledWith('http://127.0.0.1:58627');
  });

  it('never shows or opens a token when auth is bypassed', async () => {
    const deps = makeDeps();
    await handleWebCommand({ port: '58627', open: true, dangerousBypassAuth: true }, deps);

    const plain = stripAnsi(deps.writes.join(''));
    expect(plain).not.toContain('Token:');
    expect(deps.openUrl).toHaveBeenCalledWith('http://127.0.0.1:58627');
  });

  it('rejects an invalid --log-level before starting the server', async () => {
    const startServerForeground = vi.fn();
    const deps = makeDeps({ startServerForeground: startServerForeground as never });
    await expect(handleWebCommand({ port: '58627', logLevel: 'shout' }, deps)).rejects.toThrow(
      /invalid --log-level/,
    );
    expect(startServerForeground).not.toHaveBeenCalled();
  });
});
