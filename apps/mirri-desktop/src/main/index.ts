import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';

import { app, BrowserWindow, Menu, shell } from 'electron';
import type { MenuItemConstructorOptions } from 'electron';

import { ensureServer, killStaleDaemon, mirriHome, serverLogPath } from './ensure-server';
import { decideWillNavigate, decideWindowOpen } from './external-links';
import { extractDeepLinks, parseDeepLink } from './deep-link';
import { IPC_CHANNELS } from './ipc';
import { registerDesktopIpc } from './register-ipc';
import { resolveServerBinaryPath } from './server-binary';
import { createTray } from './tray';
import { isAutoUpdateEnabled, startAutoUpdate } from './updater';

let mainWindow: BrowserWindow | null = null;

/** PID of the Desktop daemon started by `ensureServer()`. Killed on app quit. */
let desktopDaemonPid: number | null = null;

/** True once the user (or the OS) asked the app to really quit — while false,
 *  closing the main window hides it to the tray instead of destroying it
 *  (background-resident desktop, D1). */
let quitting = false;

/** Deep links received before the web UI finished loading; flushed once the
 *  renderer is ready to subscribe. */
const pendingDeepLinks: string[] = [];
let rendererReady = false;

// --- window state persistence -------------------------------------------------

interface WindowBounds {
  width: number;
  height: number;
  x?: number;
  y?: number;
}

const DEFAULT_BOUNDS: WindowBounds = { width: 1280, height: 860 };

function stateFile(): string {
  return join(app.getPath('userData'), 'window-state.json');
}

function loadBounds(): WindowBounds {
  try {
    const parsed = JSON.parse(readFileSync(stateFile(), 'utf-8')) as Partial<WindowBounds>;
    if (typeof parsed.width === 'number' && typeof parsed.height === 'number') {
      return {
        width: parsed.width,
        height: parsed.height,
        x: typeof parsed.x === 'number' ? parsed.x : undefined,
        y: typeof parsed.y === 'number' ? parsed.y : undefined,
      };
    }
  } catch {
    // No saved state yet, or it is unreadable — fall back to defaults.
  }
  return DEFAULT_BOUNDS;
}

function saveBounds(win: BrowserWindow): void {
  try {
    const bounds = win.getBounds();
    mkdirSync(dirname(stateFile()), { recursive: true });
    writeFileSync(
      stateFile(),
      JSON.stringify({ width: bounds.width, height: bounds.height, x: bounds.x, y: bounds.y }),
    );
  } catch {
    // Best-effort; losing window position is not worth surfacing an error.
  }
}

// --- startup screens (no separate renderer files; inline data URLs) -----------

function dataUrl(html: string): string {
  return `data:text/html;charset=utf-8,${encodeURIComponent(html)}`;
}

const SCREEN_STYLE = `
  <style>
    html, body { height: 100%; margin: 0; }
    body {
      display: flex; flex-direction: column; align-items: center; justify-content: center;
      gap: 18px; background: #0b0b0c; color: #e7e7ea; font: 14px/1.5 system-ui, sans-serif;
      -webkit-user-select: none; user-select: none; text-align: center; padding: 0 32px;
    }
    .spinner {
      width: 34px; height: 34px; border-radius: 50%;
      border: 3px solid #2a2a2e; border-top-color: #7c8cff; animation: spin 0.9s linear infinite;
    }
    @keyframes spin { to { transform: rotate(360deg); } }
    h1 { font-size: 15px; font-weight: 600; margin: 0; }
    p { margin: 0; color: #9a9aa2; max-width: 560px; }
    code { color: #c8c8d0; word-break: break-all; }
  </style>
`;

function loadingHtml(): string {
  return `<!doctype html><meta charset="utf-8">${SCREEN_STYLE}
    <div class="spinner"></div>
    <h1>正在启动 Mirri 本地服务…</h1>
    <p>首次启动可能需要几秒。</p>`;
}

function errorHtml(message: string): string {
  const safe = message.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
  return `<!doctype html><meta charset="utf-8">${SCREEN_STYLE}
    <h1>无法启动本地服务</h1>
    <p>${safe}</p>
    <p>查看日志：<code>${serverLogPath()}</code></p>
    <p>菜单 → Mirri Code Desktop → 重试连接，或先检查日志。</p>`;
}

// --- server auth token --------------------------------------------------------

/** On-disk filename of the daemon's persistent bearer token (under MIRRICODE_HOME). */
const SERVER_TOKEN_FILE = 'server.token';

/**
 * Read the daemon's bearer token so the web UI can authenticate without showing
 * the manual token dialog on a fresh launch. Returns undefined when the token
 * cannot be read (the web UI then falls back to the dialog).
 */
function readServerToken(): string | undefined {
  try {
    const token = readFileSync(join(mirriHome(), SERVER_TOKEN_FILE), 'utf-8').trim();
    return token.length > 0 ? token : undefined;
  } catch {
    return undefined;
  }
}

// --- connect flow -------------------------------------------------------------

async function connect(win: BrowserWindow): Promise<void> {
  await win.loadURL(dataUrl(loadingHtml()));
  try {
    const { origin, pid, logPath } = await ensureServer(resolveServerBinaryPath());
    desktopDaemonPid = pid;
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
        `${origin}/?mirri_desktop=1&platform=${process.platform}${fragment}`,
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
  const win = new BrowserWindow({
    ...loadBounds(),
    minWidth: 720,
    minHeight: 480,
    backgroundColor: '#0b0b0c',
    title: 'Mirri Code Desktop',
    // macOS: hide the native title bar and float the traffic lights over the
    // content; the web UI reserves a draggable strip at the top to clear them.
    // 'default' on other platforms (they keep their native title bar).
    titleBarStyle: process.platform === 'darwin' ? 'hiddenInset' : 'default',
    webPreferences: {
      contextIsolation: true,
      nodeIntegration: false,
      preload: join(__dirname, 'preload.cjs'),
    },
  });
  mainWindow = win;
  // macOS: nudge the traffic lights down so they sit vertically centered in
  // the 48px sidebar / chat headers (hiddenInset parks them higher by default).
  if (process.platform === 'darwin') {
    const pos = win.getWindowButtonPosition();
    win.setWindowButtonPosition({ x: pos?.x ?? 20, y: 16 });
  }
  // The renderer is ready for main→renderer events once the web UI finished
  // loading — flush any deep links that queued up during startup.
  win.webContents.on('did-finish-load', () => {
    rendererReady = true;
    flushPendingDeepLinks();
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
    if (!quitting && !win.isDestroyed()) {
      event.preventDefault();
      win.hide();
    }
  });
  win.on('closed', () => {
    if (mainWindow === win) {
      mainWindow = null;
      rendererReady = false;
    }
  });
  void connect(win);
}

// --- window / deep-link plumbing ------------------------------------------------

function showMainWindow(): void {
  if (mainWindow === null || mainWindow.isDestroyed()) {
    createWindow();
    return;
  }
  if (mainWindow.isMinimized()) {
    mainWindow.restore();
  }
  mainWindow.show();
  mainWindow.focus();
}

function forwardToRenderer(channel: string, value: string): void {
  if (mainWindow !== null && !mainWindow.isDestroyed() && rendererReady) {
    mainWindow.webContents.send(channel, value);
    return;
  }
  pendingDeepLinks.push(value);
}

function flushPendingDeepLinks(): void {
  const queued = pendingDeepLinks.splice(0);
  for (const url of queued) {
    if (mainWindow !== null && !mainWindow.isDestroyed()) {
      mainWindow.webContents.send(IPC_CHANNELS.deepLink, url);
    }
  }
}

/** Handle one `mirricode://` URL: focus the window and hand the URL to the
 *  web UI (which parses the session id and switches sessions). Unknown
 *  shapes are ignored — a malformed link must never break focus handling. */
function handleDeepLink(url: string): void {
  if (parseDeepLink(url) === null) return;
  showMainWindow();
  forwardToRenderer(IPC_CHANNELS.deepLink, url);
}

/** Claim the mirricode:// OS association. Dev builds don't register — the
 *  bare electron binary would hijack the developer's links; an env override
 *  exists for manual testing of the flow in dev. */
function maybeRegisterProtocol(): void {
  if (app.isPackaged || process.env['MIRRICODE_DESKTOP_PROTOCOL_DEV'] === '1') {
    app.setAsDefaultProtocolClient('mirricode');
  }
}

// --- native menu --------------------------------------------------------------

function buildMenu(): void {
  const isMac = process.platform === 'darwin';
  const appMenu: MenuItemConstructorOptions = {
    label: 'Mirri Code Desktop',
    submenu: [
      ...(isMac ? [{ role: 'about' as const }, { type: 'separator' as const }] : []),
      {
        label: '重试连接',
        click: () => {
          if (mainWindow !== null) {
            void connect(mainWindow);
          } else {
            createWindow();
          }
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

// --- app lifecycle ------------------------------------------------------------

function main(): void {
  // Single-instance lock (D5): a second launch focuses the existing window
  // (and replays any deep link it carried) instead of spawning a second app
  // + daemon. The ensure-server port/stale-kill logic is untouched — it stays
  // the fallback for a crashed previous instance.
  const gotLock = app.requestSingleInstanceLock();
  if (!gotLock) {
    app.quit();
    return;
  }
  app.on('second-instance', (_event, argv) => {
    showMainWindow();
    for (const url of extractDeepLinks(argv)) {
      handleDeepLink(url);
    }
  });

  // Kill any stale daemon from a crashed previous Desktop session before
  // starting a new one. The PID is health-checked to avoid killing an
  // unrelated process (PID reuse).
  void killStaleDaemon();

  // macOS deep links arrive as `open-url` events — possibly before app ready.
  app.on('open-url', (event, url) => {
    event.preventDefault();
    handleDeepLink(url);
  });

  // The Desktop daemon is separate from the CLI's shared daemon — we kill
  // it on quit because no other client uses it. The CLI daemon (if running)
  // is untouched (different lock file).
  app.on('before-quit', () => {
    quitting = true;
    if (desktopDaemonPid !== null) {
      try {
        process.kill(desktopDaemonPid, 'SIGTERM');
        process.stdout.write(
          `[mirri-desktop] killed desktop daemon (pid=${desktopDaemonPid}) on quit\n`,
        );
      } catch (error) {
        process.stderr.write(
          `[mirri-desktop] failed to kill desktop daemon (pid=${desktopDaemonPid}) on quit: ${error instanceof Error ? error.message : String(error)}\n`,
        );
      }
      desktopDaemonPid = null;
    }
  });

  // Background residency (D1): the app stays alive with zero windows on
  // every platform now — the tray owns the quit action (macOS never quits
  // here anyway, and close is intercepted to hide; this handler only runs
  // during a real quit or if a window is ever destroyed unexpectedly).
  app.on('window-all-closed', () => {
    // Stay resident — quit flows through the tray menu / app menu instead.
  });

  void app.whenReady().then(() => {
    // IPC bridge + tray + protocol + updater readiness, then the window.
    registerDesktopIpc({
      focusMainWindow: () => {
        showMainWindow();
      },
      onNotificationClick: (tag) => {
        if (tag !== undefined && mainWindow !== null && !mainWindow.isDestroyed()) {
          mainWindow.webContents.send(IPC_CHANNELS.notificationClick, tag);
        }
      },
    });
    createTray({
      showMainWindow: () => {
        showMainWindow();
      },
      quit: () => {
        app.quit();
      },
    });
    maybeRegisterProtocol();

    // Auto-update technical readiness (D2): off unless the env feed URL is
    // set on a packaged build — no real distribution channel is wired yet.
    if (isAutoUpdateEnabled(process.env, app.isPackaged)) {
      const url = process.env['MIRRICODE_DESKTOP_UPDATE_URL'];
      if (url !== undefined) void startAutoUpdate(url);
    }

    buildMenu();
    createWindow();
    // Windows/Linux cold start via deep link: the OS passes the URL in argv.
    for (const url of extractDeepLinks(process.argv)) {
      handleDeepLink(url);
    }
    app.on('activate', () => {
      showMainWindow();
    });
  });
}

main();
