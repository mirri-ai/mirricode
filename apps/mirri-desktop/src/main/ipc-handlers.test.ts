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
  it('should launch the mapped app command', async () => {
    const deps = makeDeps();
    await expect(
      channelCaller(IPC_CHANNELS.openInApp, deps)({ appId: 'vscode', path: '/ws' }),
    ).resolves.toEqual({ opened: true });
    expect(deps.launch).toHaveBeenCalledWith({
      command: 'open',
      args: ['-a', 'Visual Studio Code', '/ws'],
    });
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
