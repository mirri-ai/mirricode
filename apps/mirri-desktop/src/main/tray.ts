// apps/mirri-desktop/src/main/tray.ts
//
// Tray wiring: builds the icon + menu from the pure tray-menu template and
// keeps the checkbox in sync with the real login-item state. Not unit-tested
// (Electron surface); the shape is covered by tray-menu.test.ts.

import { join } from 'node:path';

import { app, Menu, nativeImage, Tray } from 'electron';

import { buildTrayMenuTemplate } from './tray-menu';
import { trayIconSize } from './platform';

export interface TrayOptions {
  showMainWindow(): void;
  quit(): void;
}

/** The tray icon source — `build/icon.png` sits one level above the bundled
 *  `out/` directory both in dev and inside the packaged asar. */
export function trayIconPath(): string {
  return join(__dirname, '..', 'build', 'icon.png');
}

export function createTray(options: TrayOptions): Tray {
  const base = nativeImage.createFromPath(trayIconPath());
  // macOS tray slots are 16 points (32px for retina crispness); Windows and
  // Linux tray icons are 16px.
  const size = trayIconSize();
  const tray = new Tray(base.isEmpty() ? base : base.resize({ width: size, height: size }));
  tray.setToolTip('Mirri Code Desktop');

  const rebuild = (): void => {
    tray.setContextMenu(
      Menu.buildFromTemplate(
        buildTrayMenuTemplate(
          {
            onShowMainWindow: () => {
              options.showMainWindow();
            },
            onToggleLoginItem: (open) => {
              app.setLoginItemSettings({ openAtLogin: open });
              rebuild();
            },
            onQuit: () => {
              options.quit();
            },
          },
          { loginItemOn: app.getLoginItemSettings().openAtLogin },
        ),
      ),
    );
  };
  rebuild();

  // Windows/Linux: a plain tray click shows the window (macOS shows the
  // menu on click by default; 'click' there also fires, showing is
  // acceptable — it matches the "click tray to restore" idiom).
  tray.on('click', () => {
    options.showMainWindow();
  });
  return tray;
}
