// apps/mirri-desktop/src/main/menu.ts
//
// Native application menu, extracted from index.ts. Only the retry-connect
// entry needs main-process state (the window + connect flow) — it is injected
// as a callback so this module stays free of window lifecycle knowledge.
// Everything else is static Electron roles and Chinese labels, byte-identical
// to the template that lived in index.ts.

import { Menu, shell } from 'electron';

import type { MenuItemConstructorOptions } from 'electron';

import { isDarwin } from './platform';
import { serverLogPath } from './server/daemon';

/** Set the application menu. `retryConnect` reconnects the current window
 *  (or recreates it when none exists) — index.ts owns that decision. */
export function buildMenu(retryConnect: () => void): void {
  const isMac = isDarwin();
  const appMenu: MenuItemConstructorOptions = {
    label: 'Mirri Code Desktop',
    submenu: [
      ...(isMac ? [{ role: 'about' as const }, { type: 'separator' as const }] : []),
      {
        label: '重试连接',
        click: () => {
          retryConnect();
        },
      },
      {
        label: '打开服务日志',
        click: () => {
          void shell.openPath(serverLogPath());
        },
      },
      { type: 'separator' },
      isMac ? { role: 'quit' } : { role: 'close' },
    ],
  };

  const template: MenuItemConstructorOptions[] = [
    appMenu,
    { role: 'editMenu' },
    {
      label: 'View',
      submenu: [
        { role: 'reload' },
        { role: 'forceReload' },
        { role: 'toggleDevTools' },
        { type: 'separator' },
        { role: 'resetZoom' },
        { role: 'zoomIn' },
        { role: 'zoomOut' },
        { type: 'separator' },
        { role: 'togglefullscreen' },
      ],
    },
    { role: 'windowMenu' },
  ];

  Menu.setApplicationMenu(Menu.buildFromTemplate(template));
}
