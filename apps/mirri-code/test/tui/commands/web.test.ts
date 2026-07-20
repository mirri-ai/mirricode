import { beforeEach, describe, expect, it, vi } from 'vitest';

import type { StartForegroundHooks } from '#/cli/sub/server/run';
import { findBuiltInSlashCommand, resolveSlashCommandAvailability } from '#/tui/commands/index';
import type { SlashCommandHost } from '#/tui/commands/dispatch';
import { handleWebCommand, webSessionUrl } from '#/tui/commands/web';

const mocks = vi.hoisted(() => ({
  ensureDaemon: vi.fn(),
  findReusableDaemon: vi.fn(),
  startServerForeground: vi.fn(),
  tryResolveServerToken: vi.fn(),
  getDataDir: vi.fn(() => '/tmp/mirri-home'),
  openUrl: vi.fn(),
}));

vi.mock('#/cli/sub/server/daemon', async (importOriginal) => {
  const actual = await importOriginal<typeof import('#/cli/sub/server/daemon')>();
  return {
    ...actual,
    ensureDaemon: mocks.ensureDaemon,
    findReusableDaemon: mocks.findReusableDaemon,
  };
});

vi.mock('#/cli/sub/server/run', async (importOriginal) => {
  const actual = await importOriginal<typeof import('#/cli/sub/server/run')>();
  return { ...actual, startServerForeground: mocks.startServerForeground };
});

vi.mock('#/cli/sub/server/shared', async (importOriginal) => {
  const actual = await importOriginal<typeof import('#/cli/sub/server/shared')>();
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
    mocks.ensureDaemon.mockResolvedValue({
      origin: 'http://127.0.0.1:58627',
      reused: false,
      host: '127.0.0.1',
      port: 58627,
    });
    mocks.findReusableDaemon.mockResolvedValue(undefined);
  });

  describe('--background', () => {
    it('shows the token in green and opens the deep link carrying the token fragment', async () => {
      mocks.tryResolveServerToken.mockReturnValue('tok-1');
      const { host, getMountedPanel } = makeHost();

      const pending = handleWebCommand(host, '--background');
      getMountedPanel()?.handleInput('\r');
      await pending;

      expect(host.showStatus).toHaveBeenCalledWith('Starting Mirri server and opening web UI…');
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
      expect(mocks.ensureDaemon).toHaveBeenCalledOnce();
      expect(mocks.startServerForeground).not.toHaveBeenCalled();
      expect(host.setExitForegroundTask).not.toHaveBeenCalled();
      expect(host.stop).toHaveBeenCalledOnce();
    });

    it('resolves the token after the daemon is up so first-time starts carry it', async () => {
      // A fresh server writes `server.token` during startup; resolving any
      // earlier (e.g. right after the confirm dialog) would miss it.
      const callOrder: string[] = [];
      mocks.ensureDaemon.mockImplementation(async () => {
        callOrder.push('ensureDaemon');
        return { origin: 'http://127.0.0.1:58627', reused: false, host: '127.0.0.1', port: 58627 };
      });
      mocks.tryResolveServerToken.mockImplementation(() => {
        callOrder.push('resolveToken');
        return 'tok-1';
      });
      const { host, getMountedPanel } = makeHost();

      const pending = handleWebCommand(host, '--background');
      getMountedPanel()?.handleInput('\r');
      await pending;

      expect(callOrder).toEqual(['ensureDaemon', 'resolveToken']);
      expect(mocks.openUrl).toHaveBeenCalledWith(
        'http://127.0.0.1:58627/sessions/ses-1#token=tok-1',
      );
    });

    it('skips the token line and fragment when no token is available', async () => {
      mocks.tryResolveServerToken.mockReturnValue(undefined);
      const { host, getMountedPanel } = makeHost();

      const pending = handleWebCommand(host, '--background');
      getMountedPanel()?.handleInput('\r');
      await pending;

      expect(host.showStatus).toHaveBeenCalledWith('Starting Mirri server and opening web UI…');
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

    it('warns about a version mismatch when the reused daemon is from another CLI version', async () => {
      mocks.tryResolveServerToken.mockReturnValue('tok-1');
      mocks.ensureDaemon.mockResolvedValue({
        origin: 'http://127.0.0.1:58627',
        reused: true,
        host: '127.0.0.1',
        port: 58627,
        hostVersion: '9.9.9-test-old',
      });
      const { host, getMountedPanel } = makeHost();

      const pending = handleWebCommand(host, '--background');
      getMountedPanel()?.handleInput('\r');
      await pending;

      expect(host.showStatus).toHaveBeenCalledWith(
        expect.stringContaining('Running server is version 9.9.9-test-old'),
        'warning',
      );
    });

    it('describes the background daemon in the confirmation step', async () => {
      const { host, getMountedPanel } = makeHost();

      const pending = handleWebCommand(host, '--background');
      const rendered = getMountedPanel()?.render(120).join('\n') ?? '';
      getMountedPanel()?.handleInput('\r');
      await pending;

      expect(rendered).toContain('background daemon');
    });
  });

  describe('default (foreground)', () => {
    it('reuses an already-running server instead of starting a foreground one', async () => {
      mocks.tryResolveServerToken.mockReturnValue('tok-1');
      mocks.findReusableDaemon.mockResolvedValue({
        origin: 'http://127.0.0.1:58627',
        reused: true,
        host: '127.0.0.1',
        port: 58627,
      });
      const { host, getMountedPanel } = makeHost();

      const pending = handleWebCommand(host, '');
      getMountedPanel()?.handleInput('\r');
      await pending;

      expect(mocks.openUrl).toHaveBeenCalledWith(
        'http://127.0.0.1:58627/sessions/ses-1#token=tok-1',
      );
      expect(host.setExitOpenUrl).toHaveBeenCalledWith(
        'http://127.0.0.1:58627/sessions/ses-1#token=tok-1',
      );
      expect(host.stop).toHaveBeenCalledOnce();
      expect(host.setExitForegroundTask).not.toHaveBeenCalled();
      expect(mocks.startServerForeground).not.toHaveBeenCalled();
      expect(mocks.ensureDaemon).not.toHaveBeenCalled();
    });

    it('warns about a version mismatch when the reused server is from another CLI version', async () => {
      mocks.tryResolveServerToken.mockReturnValue('tok-1');
      mocks.findReusableDaemon.mockResolvedValue({
        origin: 'http://127.0.0.1:58627',
        reused: true,
        host: '127.0.0.1',
        port: 58627,
        hostVersion: '9.9.9-test-old',
      });
      const { host, getMountedPanel } = makeHost();

      const pending = handleWebCommand(host, '');
      getMountedPanel()?.handleInput('\r');
      await pending;

      expect(host.showStatus).toHaveBeenCalledWith(
        expect.stringContaining('Running server is version 9.9.9-test-old'),
        'warning',
      );
      expect(mocks.openUrl).toHaveBeenCalledWith(
        'http://127.0.0.1:58627/sessions/ses-1#token=tok-1',
      );
      expect(host.setExitForegroundTask).not.toHaveBeenCalled();
    });

    it('registers a foreground exit task that starts the server and opens the deep link', async () => {
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

      expect(mocks.startServerForeground).not.toHaveBeenCalled();
      expect(host.setExitOpenUrl).not.toHaveBeenCalled();
      expect(mocks.openUrl).not.toHaveBeenCalled();
      expect(mocks.ensureDaemon).not.toHaveBeenCalled();
      expect(mocks.tryResolveServerToken).not.toHaveBeenCalled();
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
        keepAlive: boolean;
        host: string;
        port: number;
      };
      expect(runOptions.keepAlive).toBe(true);
      expect(runOptions.host).toBe('127.0.0.1');
      expect(runOptions.port).toBe(58627);

      const stdoutSpy = vi.spyOn(process.stdout, 'write').mockImplementation(() => true);
      try {
        readyHooks?.onReady?.('http://127.0.0.1:58627');
        // The token is resolved inside the ready hook — after the server has
        // written `server.token` on first boot — never during the TUI phase.
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
        // Foreground servers stop with Ctrl+C, not `mirri server kill`.
        expect(written).toContain('Stop:     Ctrl+C');
        expect(written).not.toContain('mirri server kill');
      } finally {
        stdoutSpy.mockRestore();
      }
      // Keep the never-resolving task from outliving the test.
      void taskPending;
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
