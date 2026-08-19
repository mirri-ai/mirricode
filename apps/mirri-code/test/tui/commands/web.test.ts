import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { StartForegroundHooks } from '#/cli/sub/web/run';
import { findBuiltInSlashCommand, resolveSlashCommandAvailability } from '#/tui/commands/index';
import type { SlashCommandHost } from '#/tui/commands/dispatch';
import { handleWebCommand, webSessionUrl } from '#/tui/commands/web';

const mocks = vi.hoisted(() => ({
  startServerForeground: vi.fn(),
  tryResolveServerToken: vi.fn(),
  getDataDir: vi.fn(() => '/tmp/mirri-home'),
  openUrl: vi.fn(),
  fetch: vi.fn(),
}));

vi.mock('#/cli/sub/web/run', async (importOriginal) => {
  const actual = await importOriginal<typeof import('#/cli/sub/web/run')>();
  return { ...actual, startServerForeground: mocks.startServerForeground };
});

vi.mock('#/cli/sub/web/shared', async (importOriginal) => {
  const actual = await importOriginal<typeof import('#/cli/sub/web/shared')>();
  return { ...actual, tryResolveServerToken: mocks.tryResolveServerToken };
});

vi.mock('#/utils/open-url', async (importOriginal) => {
  const actual = await importOriginal<typeof import('#/utils/open-url')>();
  return { ...actual, openUrl: mocks.openUrl };
});

vi.mock('#/utils/paths', async (importOriginal) => {
  const actual = await importOriginal<typeof import('#/utils/paths')>();
  return { ...actual, getDataDir: mocks.getDataDir };
});

type MountedPanel = {
  handleInput: (data: string) => void;
  render: (width: number) => string[];
};

function stripAnsi(text: string): string {
  return text.replaceAll(/\u001B\[[0-9;]*m/g, '');
}

function makeHost() {
  let mountedPanel: MountedPanel | null = null;
  const host = {
    session: { id: 'ses-1' },
    showStatus: vi.fn(),
    showError: vi.fn(),
    mountEditorReplacement: vi.fn((panel: MountedPanel) => {
      mountedPanel = panel;
    }),
    restoreEditor: vi.fn(),
    setExitOpenUrl: vi.fn(),
    setExitForegroundTask: vi.fn(),
    stop: vi.fn(async () => {}),
  } as unknown as SlashCommandHost & {
    showStatus: ReturnType<typeof vi.fn>;
    showError: ReturnType<typeof vi.fn>;
    mountEditorReplacement: ReturnType<typeof vi.fn>;
    restoreEditor: ReturnType<typeof vi.fn>;
    setExitOpenUrl: ReturnType<typeof vi.fn>;
    setExitForegroundTask: ReturnType<typeof vi.fn>;
    stop: ReturnType<typeof vi.fn>;
  };
  return { host, getMountedPanel: () => mountedPanel };
}

/** Stub the running-server probe (`GET /api/v1/meta` on the default origin). */
function mockMetaServer(backend: 'v2' | 'v1' | null, ok = true): void {
  mocks.fetch.mockResolvedValue({
    ok,
    json: async () => (ok ? { code: 0, data: { backend } } : {}),
  });
}

describe('web slash command', () => {
  it('is registered as an always-available built-in', () => {
    const command = findBuiltInSlashCommand('web');
    expect(command).toBeDefined();
    expect(resolveSlashCommandAvailability(command!, '')).toBe('always');
  });
});

describe('handleWebCommand', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.getDataDir.mockReturnValue('/tmp/mirri-home');
    vi.stubGlobal('fetch', mocks.fetch);
    // Default: no running server → the command starts a foreground one.
    mockMetaServer(null, false);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('shows an error when there is no active session', async () => {
    const host = { ...makeHost().host, session: undefined } as unknown as SlashCommandHost;
    await handleWebCommand(host, '');
    expect(host.showError).toHaveBeenCalled();
    expect(mocks.startServerForeground).not.toHaveBeenCalled();
  });

  describe('reuse of a running v2 server', () => {
    it('opens the deep link on a healthy v2 server without starting a new one', async () => {
      mockMetaServer('v2');
      mocks.tryResolveServerToken.mockReturnValue('tok-1');
      const { host, getMountedPanel } = makeHost();

      const pending = handleWebCommand(host, '');
      getMountedPanel()?.handleInput('\r');
      await pending;

      expect(host.showStatus).toHaveBeenCalledWith(
        'open http://127.0.0.1:58627/sessions/ses-1#token=tok-1',
        'success',
      );
      expect(host.showStatus).toHaveBeenCalledWith('Token:    tok-1', 'success');
      expect(mocks.openUrl).toHaveBeenCalledWith(
        'http://127.0.0.1:58627/sessions/ses-1#token=tok-1',
      );
      expect(host.setExitOpenUrl).toHaveBeenCalledWith(
        'http://127.0.0.1:58627/sessions/ses-1#token=tok-1',
      );
      expect(host.stop).toHaveBeenCalledOnce();
      expect(host.setExitForegroundTask).not.toHaveBeenCalled();
      expect(mocks.startServerForeground).not.toHaveBeenCalled();
    });

    it('skips the token line and fragment when no token is available', async () => {
      mockMetaServer('v2');
      mocks.tryResolveServerToken.mockReturnValue(undefined);
      const { host, getMountedPanel } = makeHost();

      const pending = handleWebCommand(host, '');
      getMountedPanel()?.handleInput('\r');
      await pending;

      expect(host.showStatus).toHaveBeenCalledWith(
        'open http://127.0.0.1:58627/sessions/ses-1',
        'success',
      );
      expect(host.showStatus).not.toHaveBeenCalledWith(
        expect.stringContaining('Token:'),
        'success',
      );
      expect(mocks.openUrl).toHaveBeenCalledWith('http://127.0.0.1:58627/sessions/ses-1');
      expect(host.setExitOpenUrl).toHaveBeenCalledWith('http://127.0.0.1:58627/sessions/ses-1');
    });

    it('falls back to a foreground server when the /meta probe fails (network error or timeout)', async () => {
      mocks.fetch.mockRejectedValue(new Error('network down'));
      mocks.startServerForeground.mockImplementation(
        (_options: unknown, _hooks?: StartForegroundHooks) => new Promise<never>(() => {}),
      );
      const { host, getMountedPanel } = makeHost();

      const pending = handleWebCommand(host, '');
      getMountedPanel()?.handleInput('\r');
      await pending;

      expect(mocks.openUrl).not.toHaveBeenCalled();
      expect(host.setExitOpenUrl).not.toHaveBeenCalled();
      expect(host.setExitForegroundTask).toHaveBeenCalledOnce();
    });

    it('falls back to a foreground server when /meta answers non-JSON or without a backend', async () => {
      const malformedBodies = [
        { json: async () => { throw new SyntaxError('bad json'); } },
        { json: async () => ({ code: 0, data: {} }) },
      ] as const;
      for (const body of malformedBodies) {
        mocks.fetch.mockResolvedValue({ ok: true, ...body });
        mocks.startServerForeground.mockImplementation(
          (_options: unknown, _hooks?: StartForegroundHooks) => new Promise<never>(() => {}),
        );
        const { host, getMountedPanel } = makeHost();

        const pending = handleWebCommand(host, '');
        getMountedPanel()?.handleInput('\r');
        await pending;

        expect(host.setExitOpenUrl).not.toHaveBeenCalled();
        expect(host.setExitForegroundTask).toHaveBeenCalledOnce();
      }
    });
  });

  describe('foreground handoff', () => {
    it('starts a foreground v2 server and opens the deep link when none is running', async () => {
      mocks.tryResolveServerToken.mockReturnValue('tok-1');
      let readyHooks: StartForegroundHooks | undefined;
      mocks.startServerForeground.mockImplementation(
        (_options: unknown, hooks?: StartForegroundHooks) => {
          readyHooks = hooks;
          return new Promise<never>(() => {});
        },
      );
      const { host, getMountedPanel } = makeHost();

      const pending = handleWebCommand(host, '');
      getMountedPanel()?.handleInput('\r');
      await pending;

      expect(host.showStatus).toHaveBeenCalledWith(
        'Starting Mirri web server and opening web UI…',
      );
      // The token is resolved inside the ready hook — after the server has
      // written `server.token` on first boot — never during the TUI phase.
      expect(mocks.tryResolveServerToken).not.toHaveBeenCalled();
      expect(mocks.startServerForeground).not.toHaveBeenCalled();
      expect(host.setExitOpenUrl).not.toHaveBeenCalled();
      expect(mocks.openUrl).not.toHaveBeenCalled();
      expect(host.stop).toHaveBeenCalledOnce();
      expect(host.setExitForegroundTask).toHaveBeenCalledOnce();

      // Run the exit task the way run-shell's onExit would: it starts the
      // foreground server; the ready hook prints and opens the deep link.
      const task = host.setExitForegroundTask.mock.calls[0]?.[0] as (
        exitCode: number,
      ) => Promise<void>;
      const taskPending = task(0);
      expect(mocks.startServerForeground).toHaveBeenCalledOnce();
      const runOptions = mocks.startServerForeground.mock.calls[0]?.[0] as {
        host: string;
        port: number;
      };
      expect(runOptions.host).toBe('127.0.0.1');
      expect(runOptions.port).toBe(58627);

      const stdoutSpy = vi.spyOn(process.stdout, 'write').mockImplementation(() => true);
      try {
        readyHooks?.onReady?.('http://127.0.0.1:58627');
        expect(mocks.tryResolveServerToken).toHaveBeenCalledOnce();
        expect(mocks.openUrl).toHaveBeenCalledWith(
          'http://127.0.0.1:58627/sessions/ses-1#token=tok-1',
        );
        const written = stripAnsi(stdoutSpy.mock.calls.map((call) => String(call[0])).join(''));
        // Same ready banner as `mirri web`, plus the session deep link.
        expect(written).toContain('Mirri server ready');
        expect(written).toContain('http://127.0.0.1:58627/');
        expect(written).toContain('Token:    tok-1');
        expect(written).toContain('Session:  http://127.0.0.1:58627/sessions/ses-1#token=tok-1');
        // Foreground servers stop with Ctrl+C.
        expect(written).toContain('Stop:     Ctrl+C');
      } finally {
        stdoutSpy.mockRestore();
      }
      // Keep the never-resolving task from outliving the test.
      void taskPending;
    });

    it('does not reuse a v1 server — it starts the foreground v2 server instead', async () => {
      mockMetaServer('v1');
      mocks.startServerForeground.mockImplementation(
        (_options: unknown, _hooks?: StartForegroundHooks) => new Promise<never>(() => {}),
      );
      const { host, getMountedPanel } = makeHost();

      const pending = handleWebCommand(host, '');
      getMountedPanel()?.handleInput('\r');
      await pending;

      expect(host.setExitOpenUrl).not.toHaveBeenCalled();
      expect(mocks.openUrl).not.toHaveBeenCalled();
      expect(host.setExitForegroundTask).toHaveBeenCalledOnce();
    });

    it('describes the foreground behavior in the confirmation step', async () => {
      const { host, getMountedPanel } = makeHost();

      const pending = handleWebCommand(host, '');
      const rendered = getMountedPanel()?.render(120).join('\n') ?? '';
      getMountedPanel()?.handleInput('\r');
      await pending;

      expect(rendered).toContain('foreground');
      expect(rendered).toContain('Ctrl+C');
    });
  });
});

describe('webSessionUrl', () => {
  it('deep-links to the session under the origin', () => {
    expect(webSessionUrl('http://127.0.0.1:58627', 'abc123')).toBe(
      'http://127.0.0.1:58627/sessions/abc123',
    );
  });

  it('strips a trailing slash from the origin', () => {
    expect(webSessionUrl('http://127.0.0.1:58627/', 'abc123')).toBe(
      'http://127.0.0.1:58627/sessions/abc123',
    );
  });

  it('encodes session ids so the web UI can decode them', () => {
    expect(webSessionUrl('http://127.0.0.1:58627', 'a/b c')).toBe(
      'http://127.0.0.1:58627/sessions/a%2Fb%20c',
    );
  });

  it('carries the bearer token in the fragment so the browser authenticates on load', () => {
    expect(webSessionUrl('http://127.0.0.1:58627', 'abc123', 'tok-1')).toBe(
      'http://127.0.0.1:58627/sessions/abc123#token=tok-1',
    );
  });

  it('omits the fragment when no token is available', () => {
    expect(webSessionUrl('http://127.0.0.1:58627', 'abc123', undefined)).toBe(
      'http://127.0.0.1:58627/sessions/abc123',
    );
  });
});
