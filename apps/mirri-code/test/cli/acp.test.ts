/**
 * `mirri acp`
 *
 * Verifies the ACP sub-command routing: by default it wires the
 * v2-native `@mirri-ai/acp-server` (single-arg `runAcpServer(opts)`), and
 * with `MIRRICODE_LEGACY_FLAG=1` it falls back to the `@mirri-ai/acp-adapter`
 * harness path (`runAcpServer(harness, opts)`). Real servers are stubbed so
 * the tests never take over stdio.
 */

import { Command } from 'commander';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const nativeMock = vi.hoisted(() => ({
  runAcpServer: vi.fn(async (..._args: unknown[]) => undefined),
}));
const legacyMock = vi.hoisted(() => ({
  runAcpServer: vi.fn(async (..._args: unknown[]) => undefined),
  ACP_BUILTIN_SLASH_COMMANDS: [] as readonly unknown[],
}));

vi.mock('@mirri-ai/acp-server', () => nativeMock);
vi.mock('@mirri-ai/acp-adapter', () => legacyMock);

import { registerAcpCommand } from '#/cli/sub/acp';

class ExitCalled extends Error {
  constructor(public code: number | string | null | undefined) {
    super(`process.exit(${String(code)})`);
  }
}

describe('mirri acp (native v2 path)', () => {
  let exitSpy: ReturnType<typeof vi.spyOn>;
  let stderrSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    nativeMock.runAcpServer.mockClear();
    legacyMock.runAcpServer.mockClear();
    delete process.env['MIRRICODE_LEGACY_FLAG'];
    exitSpy = vi.spyOn(process, 'exit').mockImplementation(((code?: number | string | null) => {
      throw new ExitCalled(code);
    }) as never);
    stderrSpy = vi.spyOn(process.stderr, 'write').mockImplementation(() => true);
  });

  afterEach(() => {
    exitSpy.mockRestore();
    stderrSpy.mockRestore();
    delete process.env['MIRRICODE_LEGACY_FLAG'];
  });

  it('registers an `acp` subcommand on the program', () => {
    const program = new Command('mirri');
    registerAcpCommand(program);

    const acp = program.commands.find((c) => c.name() === 'acp');
    expect(acp).toBeDefined();
    expect(acp?.description()).toMatch(/Agent Client Protocol/);
  });

  it('invokes the v2 runAcpServer with agentInfo and exits 0 on success', async () => {
    const program = new Command('mirri').exitOverride();
    registerAcpCommand(program);

    await expect(program.parseAsync(['node', 'mirri', 'acp'])).rejects.toThrow(ExitCalled);

    expect(nativeMock.runAcpServer).toHaveBeenCalledTimes(1);
    expect(legacyMock.runAcpServer).not.toHaveBeenCalled();
    const optsArg = vi.mocked(nativeMock.runAcpServer).mock.calls[0]?.[0];
    expect(optsArg).toEqual(
      expect.objectContaining({
        agentInfo: { name: 'Mirri Code CLI', version: expect.any(String) },
      }),
    );
    expect(exitSpy).toHaveBeenCalledWith(0);
  });

  it('forwards MIRRICODE_HOME to terminalAuthEnv when set', async () => {
    const previous = process.env['MIRRICODE_HOME'];
    process.env['MIRRICODE_HOME'] = '/tmp/mirri-debug';
    try {
      const program = new Command('mirri').exitOverride();
      registerAcpCommand(program);

      await expect(program.parseAsync(['node', 'mirri', 'acp'])).rejects.toThrow(ExitCalled);

      const optsArg = vi.mocked(nativeMock.runAcpServer).mock.calls[0]?.[0];
      expect(optsArg).toEqual(
        expect.objectContaining({
          terminalAuthEnv: { MIRRICODE_HOME: '/tmp/mirri-debug' },
        }),
      );
    } finally {
      if (previous === undefined) {
        delete process.env['MIRRICODE_HOME'];
      } else {
        process.env['MIRRICODE_HOME'] = previous;
      }
    }
  });

  it('forwards process.argv[1] as terminalAuthLegacyCommand', async () => {
    const program = new Command('mirri').exitOverride();
    registerAcpCommand(program);

    await expect(program.parseAsync(['node', 'mirri', 'acp'])).rejects.toThrow(ExitCalled);

    const optsArg = vi.mocked(nativeMock.runAcpServer).mock.calls[0]?.[0] as {
      terminalAuthLegacyCommand?: string;
    };
    expect(typeof optsArg.terminalAuthLegacyCommand).toBe('string');
    expect((optsArg.terminalAuthLegacyCommand ?? '').length).toBeGreaterThan(0);
    expect(optsArg.terminalAuthLegacyCommand).toBe(process.argv[1]);
  });

  it('exits without starting the ACP server when --login is passed', async () => {
    const loginStub = vi.fn(async () => ({ providerName: 'mirri-code' }));
    vi.doMock(import('@mirri-ai/mirri-code-sdk'), async (importOriginal) => {
      const actual = await importOriginal();
      const makeHarness = () =>
        ({
          auth: { login: loginStub },
        }) as unknown as ReturnType<typeof actual.createMirriHarness>;
      return {
        ...actual,
        // `mirri acp --login` routes through the engine selector, which defaults
        // to createMirriHarnessV2 — cover it too or the login boots a real engine.
        createMirriHarness: makeHarness,
        createMirriHarnessV2: makeHarness,
      };
    });
    vi.resetModules();
    const { registerAcpCommand: freshRegister } = await import('#/cli/sub/acp');
    try {
      const program = new Command('mirri').exitOverride();
      freshRegister(program);

      await expect(program.parseAsync(['node', 'mirri', 'acp', '--login'])).rejects.toThrow(
        ExitCalled,
      );

      expect(loginStub).toHaveBeenCalledTimes(1);
      expect(nativeMock.runAcpServer).not.toHaveBeenCalled();
      expect(exitSpy).toHaveBeenCalledWith(0);
    } finally {
      vi.doUnmock('@mirri-ai/mirri-code-sdk');
      vi.resetModules();
    }
  });
});

describe('mirri acp (legacy path via MIRRICODE_LEGACY_FLAG)', () => {
  let exitSpy: ReturnType<typeof vi.spyOn>;
  let stderrSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    process.env['MIRRICODE_LEGACY_FLAG'] = '1';
    nativeMock.runAcpServer.mockClear();
    legacyMock.runAcpServer.mockClear();
    exitSpy = vi.spyOn(process, 'exit').mockImplementation(((code?: number | string | null) => {
      throw new ExitCalled(code);
    }) as never);
    stderrSpy = vi.spyOn(process.stderr, 'write').mockImplementation(() => true);
  });

  afterEach(() => {
    exitSpy.mockRestore();
    stderrSpy.mockRestore();
    delete process.env['MIRRICODE_LEGACY_FLAG'];
  });

  it('invokes the legacy adapter runAcpServer with a harness when the legacy flag is set', async () => {
    const program = new Command('mirri').exitOverride();
    registerAcpCommand(program);

    await expect(program.parseAsync(['node', 'mirri', 'acp'])).rejects.toThrow(ExitCalled);

    expect(legacyMock.runAcpServer).toHaveBeenCalledTimes(1);
    expect(nativeMock.runAcpServer).not.toHaveBeenCalled();
    const args = vi.mocked(legacyMock.runAcpServer).mock.calls[0];
    expect(args?.[0]).toBeDefined(); // harness
    expect(args?.[1]).toEqual(
      expect.objectContaining({
        agentInfo: { name: 'Mirri Code CLI', version: expect.any(String) },
      }),
    );
    expect(exitSpy).toHaveBeenCalledWith(0);
  });
});