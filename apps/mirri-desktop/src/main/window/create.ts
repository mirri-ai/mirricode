// apps/mirri-desktop/src/main/window/create.ts
//
// Main-window creation + the server connect flow, extracted from index.ts.
// All shared state (the window holder, the quitting flag, the daemon pid)
// still lives in index.ts — it is injected here as accessors so this module
// stays a pure window/server adapter and index.ts remains the single owner
// of the app lifecycle (menu retry, tray, notifications, before-quit all
// read the same refs).
//
// The deep-link wiring (`did-finish-load` → onRendererReady, `closed` →
// onWindowClosed) moves with createWindow because it is part of the window
// lifecycle, not of the dispatch queue itself (deep-links/dispatch.ts).

import { join } from 'node:path';

import { app, BrowserWindow, shell } from 'electron';

import { ensureServer, mirriHome } from '../server/daemon';
import { readServerToken } from '../server/token';
import { decideWillNavigate, decideWindowOpen } from '../external-links';
import { resolveServerBinaryPath } from '../server-binary';
import { dataUrl, errorHtml, loadingHtml } from '../screens';
import { currentPlatform, isDarwin } from '../platform';
import { createWindowState } from './state';

/** State seam between index.ts (the lifecycle owner) and this module. */
export interface WindowManagerOptions {
  /** Current main window (null after it closed; index.ts also reads it for
   *  menu retry, tray show, and notification forwarding). */
  getMainWindow(): BrowserWindow | null;
  /** Adopt (or clear) the main window reference on behalf of index.ts. */
  setMainWindow(win: BrowserWindow | null): void;
  /** True once the user (or the OS) asked the app to really quit — while
   *  false, closing the window hides it to the tray instead of destroying
   *  it (background-resident desktop, D1). */
  isQuitting(): boolean;
  /** Report the PID of the desktop daemon started by connect() — index.ts
   *  kills it on quit (before-quit). */
  onDaemonStarted(pid: number): void;
  /** The renderer finished loading — flush queued deep links. */
  onRendererReady(): void;
  /** The main window closed — drop the renderer-ready flag. */
  onWindowClosed(): void;
}

export function createWindowManager(options: WindowManagerOptions) {
  async function connect(win: BrowserWindow): Promise<void> {
    await win.loadURL(dataUrl(loadingHtml()));
    try {
      const { origin, pid, logPath } = await ensureServer(resolveServerBinaryPath());
      options.onDaemonStarted(pid);
      const port = Number(new URL(origin).port);
      process.stdout.write(
        `[mirri-desktop] server started: pid=${pid}, port=${port}, origin=${origin}\n` +
          `[mirri-desktop] server log: ${logPath}\n` +
          `[mirri-desktop] server state: ${mirriHome()}/server/desktop-state.json\n`,
      );
      if (!win.isDestroyed()) {
        // Append a desktop marker so the web UI shows the internal-build banner
        // even when it is served by an already-running shared daemon (the desktop
        // reuses the local daemon rather than starting a private one). Carry the
        // server token in the `#token=` fragment — like `mirri web` does — so the
        // web UI can authenticate without falling into the manual token dialog on
        // a fresh launch.
        const token = readServerToken();
        const fragment = token === undefined ? '' : `#token=${encodeURIComponent(token)}`;
        await win.loadURL(
          `${origin}/?mirri_desktop=1&platform=${currentPlatform()}${fragment}`,
        );
      }
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      process.stderr.write(`[mirri-desktop] ensureServer failed: ${message}\n`);
      if (!win.isDestroyed()) {
        await win.loadURL(dataUrl(errorHtml(message)));
      }
    }
  }

  function createWindow(): void {
    // Window-bounds persistence (window-state.json under userData), restored on
    // launch and persisted on close.
    const { loadBounds, saveBounds } = createWindowState(
      join(app.getPath('userData'), 'window-state.json'),
    );
    const win = new BrowserWindow({
      ...loadBounds(),
      minWidth: 720,
      minHeight: 480,
      backgroundColor: '#0b0b0c',
      title: 'Mirri Code Desktop',
      // macOS: hide the native title bar and float the traffic lights over the
      // content; the web UI reserves a draggable strip at the top to clear them.
      // 'default' on other platforms (they keep their native title bar).
      titleBarStyle: isDarwin() ? 'hiddenInset' : 'default',
      webPreferences: {
        contextIsolation: true,
        nodeIntegration: false,
        preload: join(__dirname, 'preload.cjs'),
      },
    });
    options.setMainWindow(win);
    // macOS: nudge the traffic lights down so they sit vertically centered in
    // the 48px sidebar / chat headers (hiddenInset parks them higher by default).
    if (isDarwin()) {
      const pos = win.getWindowButtonPosition();
      win.setWindowButtonPosition({ x: pos?.x ?? 20, y: 16 });
    }
    // The renderer is ready for main→renderer events once the web UI finished
    // loading — flush any deep links that queued up during startup.
    win.webContents.on('did-finish-load', () => {
      options.onRendererReady();
    });
    // Keep the window title as the product name. The web page sets document.title
    // ("Mirri Code Web"), which would otherwise replace it.
    win.webContents.on('page-title-updated', (event) => {
      event.preventDefault();
    });
    // Forward external links (target="_blank", window.open) to the system
    // default browser instead of opening an in-app Electron window (which has
    // no cookies / session). Always deny the in-app window.
    win.webContents.setWindowOpenHandler(({ url }) => {
      const { openExternally } = decideWindowOpen(url);
      if (openExternally) {
        void shell.openExternal(url);
      }
      return { action: 'deny' };
    });
    // Block top-level navigations that would leave the web UI origin, forwarding
    // external http(s) URLs to the system browser. Same-origin navigations
    // (web UI internal routing) are allowed through.
    win.webContents.on('will-navigate', (event, url) => {
      const currentOrigin = new URL(win.webContents.getURL()).origin;
      const { preventDefault, openExternally } = decideWillNavigate(url, currentOrigin);
      if (preventDefault) {
        event.preventDefault();
        if (openExternally) {
          void shell.openExternal(url);
        }
      }
    });
    win.on('close', (event) => {
      saveBounds(win);
      // Background residency (D1): closing hides to the tray on every platform
      // instead of destroying the window — the SPA state survives, and the
      // tray / dock / deep link can restore it instantly. A real quit (menu,
      // tray, or second-instance quit) sets `quitting` first and sails through.
      if (!options.isQuitting() && !win.isDestroyed()) {
        event.preventDefault();
        win.hide();
      }
    });
    win.on('closed', () => {
      if (options.getMainWindow() === win) {
        options.setMainWindow(null);
        options.onWindowClosed();
      }
    });
    void connect(win);
  }

  function showMainWindow(): void {
    const win = options.getMainWindow();
    if (win === null || win.isDestroyed()) {
      createWindow();
      return;
    }
    if (win.isMinimized()) {
      win.restore();
    }
    win.show();
    win.focus();
  }

  return { connect, createWindow, showMainWindow };
}
