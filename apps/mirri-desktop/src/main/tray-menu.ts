// apps/mirri-desktop/src/main/tray-menu.ts
//
// Tray menu template builder — pure (no electron import) so the shape is
// unit-testable; tray.ts builds the actual Tray and rebuilds this template
// whenever the login-item state changes.

import type { MenuItemConstructorOptions } from 'electron';

export interface TrayMenuHandlers {
  onShowMainWindow(): void;
  onToggleLoginItem(open: boolean): void;
  onQuit(): void;
}

export function buildTrayMenuTemplate(
  handlers: TrayMenuHandlers,
  state: { loginItemOn: boolean },
): MenuItemConstructorOptions[] {
  return [
    {
      label: '显示主窗口',
      click: () => {
        handlers.onShowMainWindow();
      },
    },
    {
      label: '开机自动启动',
      type: 'checkbox',
      checked: state.loginItemOn,
      click: (menuItem) => {
        handlers.onToggleLoginItem(menuItem?.checked === true);
      },
    },
    { type: 'separator' },
    {
      label: '退出 Mirri Code',
      click: () => {
        handlers.onQuit();
      },
    },
  ];
}
