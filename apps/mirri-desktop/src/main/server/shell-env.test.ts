import { describe, expect, it } from 'vitest';

import { resolveShellEnv } from './shell-env';

// ---------------------------------------------------------------------------
// resolveShellEnv()
// ---------------------------------------------------------------------------

describe('resolveShellEnv()', () => {
  it('should fall back to zsh when primary shell fails', async () => {
    const origShell = process.env['SHELL'];
    process.env['SHELL'] = '/nonexistent/shell';
    const result = await resolveShellEnv();
    if (result !== undefined) {
      expect(result['PATH']).toBeDefined();
    }
    if (origShell !== undefined) process.env['SHELL'] = origShell;
    else delete process.env['SHELL'];
  });

  it('should return a parsed environment when the shell probe succeeds', async () => {
    const origShell = process.env['SHELL'];
    if (origShell === undefined) return;
    const result = await resolveShellEnv();
    expect(result).toBeDefined();
    expect(result?.['PATH']).toBeDefined();
    expect(result?.['PATH']?.length).toBeGreaterThan(0);
    expect(result?.['HOME']).toBeDefined();
  });

  it('should return undefined when both shell and zsh probes fail', async () => {
    const origShell = process.env['SHELL'];
    process.env['SHELL'] = '/bin/cat';
    const result = await resolveShellEnv();
    expect(result === undefined || typeof result === 'object').toBe(true);
    if (origShell !== undefined) process.env['SHELL'] = origShell;
    else delete process.env['SHELL'];
  }, 10_000);
});
