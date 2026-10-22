// apps/mirri-desktop/src/main/deep-links/dispatch.ts
//
// Deep-link dispatch: the queue + forward + flush plumbing for `mirricode://`
// links, extracted from index.ts. The renderer-side SPA cannot receive
// main→renderer events until it finishes loading, so links that arrive during
// startup (cold-start argv, `open-url`, second-instance) are queued and
// flushed on the renderer-ready signal.
//
// The queue/forward/flush logic is pure — the window reference and the
// focus side effect are injected (`getMainWindow` / `showMainWindow`) so the
// behavior is unit-testable and index.ts keeps owning the window lifecycle.
// `maybeRegisterProtocol` is the one Electron-touching piece (it reads
// `app.isPackaged` / `app.setAsDefaultProtocolClient`); it lives here because
// it is part of the OS deep-link association, not the window lifecycle.

import { app } from 'electron';

import type { BrowserWindow } from 'electron';

import { parseDeepLink } from '../deep-link';
import { IPC_CHANNELS } from '../ipc';

export interface DeepLinkDispatchOptions {
  /** Current main window (null after it closed; the shell may recreate it). */
  getMainWindow(): BrowserWindow | null;
  /** Bring the app to the front — called for every valid link, even when the
   *  URL must be queued for a not-yet-ready renderer. */
  showMainWindow(): void;
}

export function createDeepLinkDispatch(options: DeepLinkDispatchOptions) {
  const pendingDeepLinks: string[] = [];
  let rendererReady = false;

  function forwardToRenderer(channel: string, value: string): void {
    const win = options.getMainWindow();
    if (win !== null && !win.isDestroyed() && rendererReady) {
      win.webContents.send(channel, value);
      return;
    }
    pendingDeepLinks.push(value);
  }

  function flushPendingDeepLinks(): void {
    const queued = pendingDeepLinks.splice(0);
    for (const url of queued) {
      const win = options.getMainWindow();
      if (win !== null && !win.isDestroyed()) {
        win.webContents.send(IPC_CHANNELS.deepLink, url);
      }
    }
  }

  /** Handle one `mirricode://` URL: focus the window and hand the URL to the
   *  web UI (which parses the session id and switches sessions). Unknown
   *  shapes are ignored — a malformed link must never break focus handling. */
  function handleDeepLink(url: string): void {
    if (parseDeepLink(url) === null) return;
    options.showMainWindow();
    forwardToRenderer(IPC_CHANNELS.deepLink, url);
  }

  return {
    handleDeepLink,
    /** The renderer finished loading — flush links queued during startup. */
    onRendererReady(): void {
      rendererReady = true;
      flushPendingDeepLinks();
    },
    /** The main window closed — drop the ready flag so links arriving before
     *  the next window's renderer loads queue again. */
    onWindowClosed(): void {
      rendererReady = false;
    },
  };
}

/** Claim the mirricode:// OS association. Dev builds don't register — the
 *  bare electron binary would hijack the developer's links; an env override
 *  exists for manual testing of the flow in dev. */
export function maybeRegisterProtocol(): void {
  if (app.isPackaged || process.env['MIRRICODE_DESKTOP_PROTOCOL_DEV'] === '1') {
    app.setAsDefaultProtocolClient('mirricode');
  }
}
