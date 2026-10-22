// apps/mirri-desktop/src/main/register-ipc.ts
//
// Wires the unit-tested handler factory (ipc-handlers.ts) to Electron's real
// shell / Notification / app surfaces and registers every channel on
// ipcMain. Not unit-tested — the logic lives in the factory; this file is
// only the Electron adapter.

import { existsSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

import { app, ipcMain, Notification, shell } from 'electron';

import { readAppBundleIcon } from './app-icon';
import { IPC_CHANNELS } from './ipc';
import { createDesktopIpcHandlers, type DesktopIpcDeps } from './ipc-handlers';
import { launchDetached } from './file-launch';
import { currentPlatform, isDarwin } from './platform';
import { detectInstalledOpenInApps } from './open-in-apps';

export interface RegisterIpcOptions {
  /** Called when the user clicks a native notification identified by `tag`
   *  (undefined when the notification carried no tag). */
  onNotificationClick(tag: string | undefined): void;
  /** Bring the main window to the front (notification click implies focus). */
  focusMainWindow(): void;
}

export function registerDesktopIpc(options: RegisterIpcOptions): void {
  const deps: DesktopIpcDeps = {
    shellOpenPath: (path) => shell.openPath(path),
    shellRevealPath: (path) => shell.showItemInFolder(path),
    shellOpenExternal: (url) => shell.openExternal(url),
    launch: (cmd) => launchDetached(cmd),
    showNotification: (title, body, tag) => {
      if (!Notification.isSupported()) return false;
      try {
        const notification = new Notification({ title, body });
        notification.on('click', () => {
          options.focusMainWindow();
          options.onNotificationClick(tag);
        });
        notification.show();
        return true;
      } catch {
        return false;
      }
    },
    detectOpenInApps: () =>
      detectInstalledOpenInApps({
        platform: currentPlatform(),
        existsSync,
        applicationsDirs: ['/Applications', join(homedir(), 'Applications')],
        getAppBundleIcon: (bundlePath) => readAppBundleIcon(bundlePath),
      }),
    setBadgeCount: (count) => app.setBadgeCount(count),    dockBounce: (kind) => {
      if (isDarwin() && app.dock !== undefined) {
        app.dock.bounce(kind);
      }
    },
    setLoginItem: (open) => {
      app.setLoginItemSettings({ openAtLogin: open });
    },
    getLoginItem: () => app.getLoginItemSettings().openAtLogin,
    env: process.env,
    platform: currentPlatform(),
  };

  const handlers = createDesktopIpcHandlers(deps);
  for (const [channel, handler] of Object.entries(handlers)) {
    ipcMain.handle(channel, (_event, payload) => handler(payload));
  }
}

export { IPC_CHANNELS };
