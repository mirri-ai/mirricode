import { describe, expect, it, vi } from 'vitest';

import { IPC_CHANNELS } from './ipc';
import { createDesktopIpcHandlers, type DesktopIpcDeps } from './ipc-handlers';
import type { LaunchCommand } from './file-launch';

function makeDeps(overrides: Partial<DesktopIpcDeps> = {}): DesktopIpcDeps {
  return {
    shellOpenPath: vi.fn().mockResolvedValue(''),
    shellRevealPath: vi.fn().mockReturnValue(true),
    shellOpenExternal: vi.fn().mockResolvedValue(undefined),
    launch: vi.fn().mockResolvedValue(undefined),
    showNotification: vi.fn().mockReturnValue(true),
    detectOpenInApps: vi.fn().mockResolvedValue([]),
    existsSync: vi.fn().mockReturnValue(false),
    readdirSync: vi.fn().mockReturnValue([]),
    setBadgeCount: vi.fn().mockReturnValue(true),
    dockBounce: vi.fn(),
    setLoginItem: vi.fn(),
    getLoginItem: vi.fn().mockReturnValue(false),
    env: { EDITOR: 'vim' },
    platform: 'darwin',
    ...overrides,
  };
}

function channelCaller(channel: string, deps: DesktopIpcDeps) {
  const handlers = createDesktopIpcHandlers(deps);
  const handler = handlers[channel];
  if (handler === undefined) throw new Error(`no handler for ${channel}`);
  return handler;
}

describe('desktop:open-path', () => {
  it('should open a validated path with shell.openPath', async () => {
    const deps = makeDeps();
    await channelCaller(IPC_CHANNELS.openPath, deps)({ path: '/tmp/a.txt' });
    expect(deps.shellOpenPath).toHaveBeenCalledWith('/tmp/a.txt');
  });

  it('should throw when shell.openPath returns an error string', async () => {
    const deps = makeDeps({ shellOpenPath: vi.fn().mockResolvedValue('file missing') });
    await expect(
      channelCaller(IPC_CHANNELS.openPath, deps)({ path: '/tmp/a.txt' }),
    ).rejects.toThrow('file missing');
  });

  it('should reject invalid payloads', async () => {
    const deps = makeDeps();
    await expect(channelCaller(IPC_CHANNELS.openPath, deps)({ path: '' })).rejects.toThrow();
    expect(deps.shellOpenPath).not.toHaveBeenCalled();
  });
});

describe('desktop:reveal-path', () => {
  it('should reveal a validated path with shell.showItemInFolder', async () => {
    const deps = makeDeps();
    await expect(
      channelCaller(IPC_CHANNELS.revealPath, deps)({ path: '/tmp/a.txt' }),
    ).resolves.toEqual({ revealed: true });
    expect(deps.shellRevealPath).toHaveBeenCalledWith('/tmp/a.txt');
  });
});

describe('desktop:open-in-editor', () => {
  it('should launch the configured editor with the line target', async () => {
    const deps = makeDeps({ env: { EDITOR: 'code' } });
    await expect(
      channelCaller(IPC_CHANNELS.openInEditor, deps)({ path: '/tmp/a.rs', line: 3 }),
    ).resolves.toEqual({ opened: true });
    expect(deps.launch).toHaveBeenCalledWith({
      command: "code '/tmp/a.rs:3'",
      args: [],
      shell: true,
    });
  });

  it('should fall back to shell.openPath when no editor is configured', async () => {
    const deps = makeDeps({ env: {} });
    await expect(
      channelCaller(IPC_CHANNELS.openInEditor, deps)({ path: '/tmp/a.rs' }),
    ).resolves.toEqual({ opened: true });
    expect(deps.launch).not.toHaveBeenCalled();
    expect(deps.shellOpenPath).toHaveBeenCalledWith('/tmp/a.rs');
  });

  it('should throw when the editor binary is missing and openPath fails', async () => {
    const deps = makeDeps({
      launch: vi.fn().mockRejectedValue(new Error('ENOENT')),
      shellOpenPath: vi.fn().mockResolvedValue('default app missing'),
    });
    await expect(
      channelCaller(IPC_CHANNELS.openInEditor, deps)({ path: '/tmp/a.rs' }),
    ).rejects.toThrow();
  });
});

describe('desktop:open-in-app', () => {
  it('should launch the resolved bundle on macOS', async () => {
    const deps = makeDeps({
      existsSync: vi.fn().mockImplementation((p: string) =>
        p === '/Applications/Visual Studio Code.app' || p === '/ws',
      ),
    });
    await expect(
      channelCaller(IPC_CHANNELS.openInApp, deps)({ appId: 'vscode', path: '/ws' }),
    ).resolves.toEqual({ opened: true });
    expect(deps.launch).toHaveBeenCalledWith({
      command: 'open',
      args: ['-a', 'Visual Studio Code', '/ws'],
    });
  });

  it('should launch the Community Edition bundle on macOS when that is what is installed', async () => {
    const deps = makeDeps({
      existsSync: vi.fn().mockImplementation((p: string) =>
        p === '/Applications/PyCharm CE.app' || p === '/ws',
      ),
    });
    await expect(
      channelCaller(IPC_CHANNELS.openInApp, deps)({ appId: 'pycharm', path: '/ws' }),
    ).resolves.toEqual({ opened: true });
    expect(deps.launch).toHaveBeenCalledWith({
      command: 'open',
      args: ['-a', 'PyCharm CE', '/ws'],
    });
  });

  it('should launch the resolved exe directly when the platform is Windows', async () => {
    const deps = makeDeps({
      platform: 'win32',
      env: { LOCALAPPDATA: 'C:\\Users\\demo\\AppData\\Local' },
      existsSync: vi
        .fn()
        .mockImplementation(
          (p: string) =>
            p === 'C:\\Users\\demo\\AppData\\Local\\Programs\\Microsoft VS Code\\Code.exe' ||
            p === 'C:\\ws',
        ),
    });
    await expect(
      channelCaller(IPC_CHANNELS.openInApp, deps)({ appId: 'vscode', path: 'C:\\ws' }),
    ).resolves.toEqual({ opened: true });
    // A GUI editor must not inherit `windowsHide`: on Windows that shows or
    // hides the window the launched process creates.
    expect(deps.launch).toHaveBeenCalledWith({
      command: 'C:\\Users\\demo\\AppData\\Local\\Programs\\Microsoft VS Code\\Code.exe',
      args: ['C:\\ws'],
      windowsHide: false,
    });
  });

  it('should launch a JetBrains IDE found through the versioned install scan on Windows', async () => {
    const root = 'C:\\Program Files\\JetBrains';
    const install = `${root}\\IntelliJ IDEA 2024.1`;
    const deps = makeDeps({
      platform: 'win32',
      env: { ProgramFiles: 'C:\\Program Files' },
      existsSync: vi
        .fn()
        .mockImplementation((p: string) => p === root || p === install || p === 'C:\\ws'),
      readdirSync: vi.fn().mockImplementation((p: string) => {
        if (p === root) return [{ name: 'IntelliJ IDEA 2024.1', isDirectory: () => true }];
        if (p === install) return [{ name: 'bin', isDirectory: () => true }];
        if (p === `${install}\\bin`) return [{ name: 'idea64.exe', isDirectory: () => false }];
        throw new Error(`ENOENT: ${p}`);
      }),
    });
    await expect(
      channelCaller(IPC_CHANNELS.openInApp, deps)({ appId: 'intellij', path: 'C:\\ws' }),
    ).resolves.toEqual({ opened: true });
    expect(deps.launch).toHaveBeenCalledWith({
      command: `${install}\\bin\\idea64.exe`,
      args: ['C:\\ws'],
      windowsHide: false,
    });
  });

  it('should pass windowsHide false when File Explorer is chosen on Windows', async () => {
    // explorer.exe forwards the inherited show state to the folder window it
    // creates, so hiding the spawn hides the window and the click looks dead.
    const deps = makeDeps({
      platform: 'win32',
      existsSync: vi.fn().mockImplementation((p: string) => p === 'C:\\my ws'),
    });
    await expect(
      channelCaller(IPC_CHANNELS.openInApp, deps)({ appId: 'finder', path: 'C:\\my ws' }),
    ).resolves.toEqual({ opened: true });
    expect(deps.launch).toHaveBeenCalledWith({
      command: 'explorer.exe',
      args: ['C:\\my ws'],
      windowsHide: false,
    });
  });

  it('should restore the lost drive prefix when a Windows root arrives driveless', async () => {
    // Records written by the old path-resolution bug kept `/Users/me/ws` after
    // dropping the drive; Windows resolves that against its own current drive,
    // which is never the user's workspace.
    const deps = makeDeps({
      platform: 'win32',
      env: { USERPROFILE: 'C:\\Users\\demo' },
      existsSync: vi.fn().mockImplementation((p: string) => p === 'C:\\Users\\demo\\ws'),
    });
    await expect(
      channelCaller(IPC_CHANNELS.openInApp, deps)({ appId: 'finder', path: '/Users/demo/ws' }),
    ).resolves.toEqual({ opened: true });
    expect(deps.launch).toHaveBeenCalledWith({
      command: 'explorer.exe',
      args: ['C:\\Users\\demo\\ws'],
      windowsHide: false,
    });
  });

  it('should refuse to launch when the target directory does not exist', async () => {
    // Both explorer.exe and editors accept a non-existent path without an
    // error, so the handler must reject instead of spawning a silent no-op.
    const deps = makeDeps({
      platform: 'win32',
      env: { USERPROFILE: 'C:\\Users\\demo' },
      existsSync: vi.fn().mockReturnValue(false),
    });
    await expect(
      channelCaller(IPC_CHANNELS.openInApp, deps)({ appId: 'finder', path: '/Users/demo/missing' }),
    ).rejects.toThrow(/not an existing directory/);
    expect(deps.launch).not.toHaveBeenCalled();
  });

  it('should reject unknown app ids', async () => {
    const deps = makeDeps();
    await expect(
      channelCaller(IPC_CHANNELS.openInApp, deps)({ appId: 'sh', path: '/ws' }),
    ).rejects.toThrow();
    expect(deps.launch).not.toHaveBeenCalled();
  });
});

describe('desktop:open-external', () => {
  it('should open validated http(s) urls with shell.openExternal', async () => {
    const deps = makeDeps();
    await expect(
      channelCaller(IPC_CHANNELS.openExternal, deps)({ url: 'https://example.com/x' }),
    ).resolves.toEqual({ opened: true });
    expect(deps.shellOpenExternal).toHaveBeenCalledWith('https://example.com/x');
  });

  it('should reject non-http(s) urls', async () => {
    const deps = makeDeps();
    await expect(
      channelCaller(IPC_CHANNELS.openExternal, deps)({ url: 'file:///etc/passwd' }),
    ).rejects.toThrow();
    expect(deps.shellOpenExternal).not.toHaveBeenCalled();
  });
});

describe('desktop:notify', () => {
  it('should show a native notification and return shown=true', async () => {
    const deps = makeDeps();
    await expect(
      channelCaller(IPC_CHANNELS.notify, deps)({ title: 'Done', body: 'turn finished' }),
    ).resolves.toEqual({ shown: true });
    expect(deps.showNotification).toHaveBeenCalledWith('Done', 'turn finished', undefined);
  });

  it('should report shown=false when notifications are unsupported', async () => {
    const deps = makeDeps({ showNotification: vi.fn().mockReturnValue(false) });
    await expect(
      channelCaller(IPC_CHANNELS.notify, deps)({ title: 'a', body: 'b' }),
    ).resolves.toEqual({ shown: false });
  });

  it('should reject invalid payloads', async () => {
    const deps = makeDeps();
    await expect(channelCaller(IPC_CHANNELS.notify, deps)({ title: '', body: 'b' })).rejects
      .toThrow();
  });
});

describe('desktop:set-badge-count', () => {
  it('should set the badge for a valid count', async () => {
    const deps = makeDeps();
    await expect(channelCaller(IPC_CHANNELS.setBadgeCount, deps)({ count: 4 })).resolves.toEqual(
      { set: true },
    );
    expect(deps.setBadgeCount).toHaveBeenCalledWith(4);
  });

  it('should reject a negative count', async () => {
    const deps = makeDeps();
    await expect(channelCaller(IPC_CHANNELS.setBadgeCount, deps)({ count: -1 })).rejects.toThrow();
  });
});

describe('desktop:dock-bounce', () => {
  it('should bounce with informational as the default kind', async () => {
    const deps = makeDeps();
    await channelCaller(IPC_CHANNELS.dockBounce, deps)({});
    expect(deps.dockBounce).toHaveBeenCalledWith('informational');
  });

  it('should pass through the critical kind', async () => {
    const deps = makeDeps();
    await channelCaller(IPC_CHANNELS.dockBounce, deps)({ kind: 'critical' });
    expect(deps.dockBounce).toHaveBeenCalledWith('critical');
  });

  it('should reject an unknown kind', async () => {
    const deps = makeDeps();
    await expect(
      channelCaller(IPC_CHANNELS.dockBounce, deps)({ kind: 'explosive' }),
    ).rejects.toThrow();
  });
});

describe('desktop:list-open-in-apps', () => {
  it('should return the detected apps when invoked', async () => {
    const deps = makeDeps({
      detectOpenInApps: async () => [
        { appId: 'finder', name: 'Finder' },
        { appId: 'zed', name: 'Zed', iconDataUrl: 'data:image/png;base64,AAA' },
      ],
    });
    await expect(channelCaller(IPC_CHANNELS.listOpenInApps, deps)(undefined)).resolves.toEqual([
      { appId: 'finder', name: 'Finder' },
      { appId: 'zed', name: 'Zed', iconDataUrl: 'data:image/png;base64,AAA' },
    ]);
  });
});

describe('desktop:set-login-item / desktop:get-login-item', () => {
  it('should set the login item from the payload', async () => {
    const deps = makeDeps();
    await expect(channelCaller(IPC_CHANNELS.setLoginItem, deps)({ open: true })).resolves.toEqual(
      { open: true },
    );
    expect(deps.setLoginItem).toHaveBeenCalledWith(true);
  });

  it('should read back the login item state', async () => {
    const deps = makeDeps({ getLoginItem: vi.fn().mockReturnValue(true) });
    await expect(channelCaller(IPC_CHANNELS.getLoginItem, deps)(undefined)).resolves.toEqual({
      open: true,
    });
  });
});
