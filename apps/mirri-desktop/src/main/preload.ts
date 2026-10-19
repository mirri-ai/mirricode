// apps/mirri-desktop/src/main/preload.ts
//
// The preload bridge: exposes the `mirriDesktop` API surface to the renderer
// via contextBridge. The renderer (the mirri-web SPA) is untrusted web
// content, so the ONLY surface it gets is the function set below — every call
// crosses into the main process where the payload is validated before any OS
// action runs (see register-ipc.ts / ipc.ts).
//
// Bundled by tsdown into out/preload.cjs (contextIsolation stays on; this
// file never gains nodeIntegration).

import { contextBridge, ipcRenderer } from 'electron';

import { IPC_CHANNELS, type DesktopBridgeApi } from './ipc';

const bridge: DesktopBridgeApi = {
  openPath: (path) => ipcRenderer.invoke(IPC_CHANNELS.openPath, { path }),
  revealPath: (path) => ipcRenderer.invoke(IPC_CHANNELS.revealPath, { path }),
  openInEditor: (path, line) => ipcRenderer.invoke(IPC_CHANNELS.openInEditor, { path, line }),
  openInApp: (appId, path) => ipcRenderer.invoke(IPC_CHANNELS.openInApp, { appId, path }),
  listOpenInApps: () => ipcRenderer.invoke(IPC_CHANNELS.listOpenInApps),
  openExternal: (url) => ipcRenderer.invoke(IPC_CHANNELS.openExternal, { url }),
  notify: (title, body, tag) => ipcRenderer.invoke(IPC_CHANNELS.notify, { title, body, tag }),
  setBadgeCount: (count) => ipcRenderer.invoke(IPC_CHANNELS.setBadgeCount, { count }),
  dockBounce: (kind) => ipcRenderer.invoke(IPC_CHANNELS.dockBounce, { kind }),
  setLoginItem: (open) => ipcRenderer.invoke(IPC_CHANNELS.setLoginItem, { open }),
  getLoginItem: () => ipcRenderer.invoke(IPC_CHANNELS.getLoginItem),
  onDeepLink: (callback) => {
    ipcRenderer.on(IPC_CHANNELS.deepLink, (_event, url: unknown) => {
      if (typeof url === 'string') callback(url);
    });
  },
  onNotificationClick: (callback) => {
    ipcRenderer.on(IPC_CHANNELS.notificationClick, (_event, tag: unknown) => {
      if (typeof tag === 'string') callback(tag);
    });
  },
};

contextBridge.exposeInMainWorld('mirriDesktop', bridge);
