// apps/mirri-desktop/src/main/index.ts
//
// Main-process entry point: the app lifecycle orchestrator only. Window
// creation + the server connect flow live in window/create.ts; deep-link
// dispatch in deep-links/dispatch.ts; the native menu in menu.ts; the
// desktop daemon plumbing in server/*. This file owns the shared state
// (main window holder, quitting flag, desktop daemon pid) and wires the
// Electron app events to the extracted modules.

import { BrowserWindow, app } from 'electron';

import type { BrowserWindow as BrowserWindowType } from 'electron';

import { DESKTOP_PORT, killStaleDaemon, shutdownDaemon } from './server/daemon';
import { extractDeepLinks } from './deep-link';
import { createDeepLinkDispatch, maybeRegisterProtocol } from './deep-links/dispatch';
import { IPC_CHANNELS } from './ipc';
import { buildMenu } from './menu';
import { registerDesktopIpc } from './register-ipc';
import { createTray } from './tray';
import { isAutoUpdateEnabled, startAutoUpdate } from './updater';
import { createWindowManager } from './window/create';

let mainWindow: BrowserWindowType | null = null;

/** PID of the Desktop daemon started by the connect flow. Killed on quit. */
let desktopDaemonPid: number | null = null;

/**
 * Grace window the daemon gets between SIGTERM and the SIGKILL escalation
 * (ms). The rust shutdown is sub-second once its in-flight turns are
 * interrupted at SIGTERM; 15s only ever elapses for a wedged drain, which
 * is exactly what the SIGKILL backstop exists for.
 */
const DAEMON_SHUTDOWN_GRACE_MS = 15_000;

/** True once the user (or the OS) asked the app to really quit — while false,
 *  closing the main window hides it to the tray instead of destroying it
 *  (background-resident desktop, D1). */
let quitting = false;

let windowManager: ReturnType<typeof createWindowManager>;

// --- deep-link dispatch -------------------------------------------------------

/** Queue + forward for `mirricode://` links (see deep-links/dispatch.ts).
 *  The window stays owned here: the dispatch focuses it through the
 *  `showMainWindow` callback and reads it through `getMainWindow`. */
const deepLinks = createDeepLinkDispatch({
  getMainWindow: () => mainWindow,
  showMainWindow: () => {
    windowManager.showMainWindow();
  },
});

windowManager = createWindowManager({
  getMainWindow: () => mainWindow,
  setMainWindow: (win) => {
    mainWindow = win;
  },
  isQuitting: () => quitting,
  onDaemonStarted: (pid) => {
    desktopDaemonPid = pid;
  },
  onRendererReady: () => {
    deepLinks.onRendererReady();
  },
  onWindowClosed: () => {
    deepLinks.onWindowClosed();
  },
});

// --- app lifecycle ------------------------------------------------------------

function main(): void {
  // Single-instance lock (D5): a second launch focuses the existing window
  // (and replays any deep link it carried) instead of spawning a second app
  // + daemon. The server module's port/stale-kill logic is untouched — it stays
  // the fallback for a crashed previous instance.
  const gotLock = app.requestSingleInstanceLock();
  if (!gotLock) {
    app.quit();
    return;
  }
  app.on('second-instance', (_event, argv) => {
    windowManager.showMainWindow();
    for (const url of extractDeepLinks(argv)) {
      deepLinks.handleDeepLink(url);
    }
  });

  // Kill any stale daemon from a crashed previous Desktop session before
  // starting a new one. The PID is health-checked to avoid killing an
  // unrelated process (PID reuse).
  void killStaleDaemon();

  // macOS deep links arrive as `open-url` events — possibly before app ready.
  app.on('open-url', (event, url) => {
    event.preventDefault();
    deepLinks.handleDeepLink(url);
  });

  // The Desktop daemon is separate from the CLI's shared daemon — we kill
  // it on quit because no other client uses it. The CLI daemon (if running)
  // is untouched (different lock file).
  //
  // The kill is a HANDSHAKE, not fire-and-forget: `before-quit` prevents
  // the default quit, stops the daemon (SIGTERM → bounded wait → SIGKILL
  // escalation, see `shutdownDaemon`), and only then re-quits. Waiting is
  // the fix for the stale-port bug: a daemon that outlives the app holds
  // 58827 and the next launch fails with "Address already in use".
  app.on('before-quit', (event) => {
    quitting = true;
    if (desktopDaemonPid === null) {
      return;
    }
    const pid = desktopDaemonPid;
    desktopDaemonPid = null;
    event.preventDefault();
    // Destroy the windows FIRST: the renderer's WebSocket to the daemon
    // keeps the daemon's HTTP graceful shutdown waiting (an upgraded
    // connection never looks idle to it), so a live window would hold the
    // exit open for the whole grace window. Destroying (not just closing)
    // tears the renderer — and its WS — down now; `quitting` is already
    // true so window-close handlers route to destruction, not hiding.
    for (const win of BrowserWindow.getAllWindows()) {
      win.destroy();
    }
    void shutdownDaemon(pid, DESKTOP_PORT, DAEMON_SHUTDOWN_GRACE_MS)
      .catch((error: unknown) => {
        process.stderr.write(
          `[mirri-desktop] desktop daemon shutdown handshake failed: ` +
            `${error instanceof Error ? error.message : String(error)}\n`,
        );
      })
      .finally(() => {
        app.quit();
      });
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
        windowManager.showMainWindow();
      },
      onNotificationClick: (tag) => {
        if (tag !== undefined && mainWindow !== null && !mainWindow.isDestroyed()) {
          mainWindow.webContents.send(IPC_CHANNELS.notificationClick, tag);
        }
      },
    });
    createTray({
      showMainWindow: () => {
        windowManager.showMainWindow();
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

    buildMenu(() => {
      if (mainWindow !== null) {
        void windowManager.connect(mainWindow);
      } else {
        windowManager.createWindow();
      }
    });
    windowManager.createWindow();
    // Windows/Linux cold start via deep link: the OS passes the URL in argv.
    for (const url of extractDeepLinks(process.argv)) {
      deepLinks.handleDeepLink(url);
    }
    app.on('activate', () => {
      windowManager.showMainWindow();
    });
  });
}

main();
