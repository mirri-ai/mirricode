// apps/mirri-desktop-lite/src/bun/index.ts — main process entry.
//
// Boot order (design §3.1): ensure daemon → create hiddenInset window with
// the bridge preload → load the daemon-served web UI. Falls back to the
// bundled self-check page when the daemon cannot be started.

import { ApplicationMenu, BrowserView, BrowserWindow } from "electrobun/main";
import { openExternal } from "electrobun/main/utils";
import { electrobunEventEmitter } from "electrobun/main/events";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { findRepoRoot } from "./daemon";

import { createBridgeHandlers } from "./bridge";
import { cleanupOwnedDaemon, defaultDaemonPaths, ensureDaemon } from "./daemon";
import { readServerToken } from "./token";

import type { MirriSchema } from "../shared/schema";

const rpc = BrowserView.defineRPC<MirriSchema>(createBridgeHandlers());

// Standard application menu (macOS): the Edit roles are what wire Cmd+X/C/V
// (and friends) through WKWebView — without an Edit menu the key equivalents
// never reach the webview, and only right-click → Paste works.
ApplicationMenu.setApplicationMenu([
  {
    label: 'Mirri Code Lite',
    submenu: [
      { role: 'about' },
      { type: 'separator' },
      { role: 'hide' },
      { role: 'hideOthers' },
      { type: 'separator' },
      { role: 'quit' },
    ],
  },
  {
    label: 'Edit',
    submenu: [
      { role: 'undo' },
      { role: 'redo' },
      { type: 'separator' },
      { role: 'cut' },
      { role: 'copy' },
      { role: 'paste' },
      { role: 'selectAll' },
    ],
  },
  {
    label: 'View',
    submenu: [
      { label: 'Reload', accelerator: 'Cmd+R', action: 'web-reload' },
      { type: 'separator' },
      { label: 'Zoom In', accelerator: 'Cmd+=', action: 'zoom-in' },
      { label: 'Zoom Out', accelerator: 'Cmd+-', action: 'zoom-out' },
      { label: 'Actual Size', accelerator: 'Cmd+0', action: 'zoom-reset' },
      { type: 'separator' },
      { role: 'toggleFullScreen' },
    ],
  },
  {
    label: 'Window',
    submenu: [
      { role: 'minimize' },
      { role: 'zoom' },
      { type: 'separator' },
      { role: 'bringAllToFront' },
      { role: 'cycleThroughWindows' },
    ],
  },
]);

// External links: `target="_blank"` (markdown links) would be silently
// dropped by WKWebView (no in-app popup windows) — forward every
// new-window request to the system browser instead. Same policy as the
// Electron shell's decideWindowOpen: http(s) only.
electrobunEventEmitter.on("new-window-open", (event: { data?: { detail?: { url?: unknown } } }) => {
  // ElectrobunEvent shape: { name, data: { detail: { url, ... } } }
  const url = event?.data?.detail?.url;
  console.log(`[lite] new-window-open: ${String(url)}`);
  if (typeof url === "string" && /^https?:\/\//i.test(url)) {
    openExternal(url);
  }
});

const daemonPaths = defaultDaemonPaths();

const win = new BrowserWindow({
  title: "Mirri Code Lite",
  url: "views://mainview/index.html",
  titleBarStyle: "hiddenInset",
  frame: { width: 1100, height: 750, x: 140, y: 140 },
  rpc: rpc as never,
  // ADR-002: preload takes the script CONTENT (a path silently fails).
  preload: readFileSync(join(import.meta.dir, "../preload.js"), "utf8"),
});

// Probe: same-origin navigations must not leak out, external ones get logged.
win.webview.on("will-navigate", (event: unknown) => {
  console.log(`[lite] will-navigate: ${JSON.stringify(event)}`);
});

// macOS: nudge the traffic lights down so they sit vertically centered in
// the 48px sidebar / chat headers (hiddenInset parks them higher by default —
// same y=16 treatment as the Electron shell's window/create.ts).
{
  const pos = win.getWindowButtonPosition();
  win.setWindowButtonPosition(pos?.x ?? 20, 16);
}

// Cmd+= / Cmd+- / Cmd+0 page zoom (WKWebView setPageZoom; 1.0 = 100%).
const ZOOM_MIN = 0.5;
const ZOOM_MAX = 2.0;
let pageZoom = 1.0;
ApplicationMenu.on("application-menu-clicked", (event: unknown) => {
  const action = (event as { data?: { action?: unknown } } | undefined)?.data?.action;
  if (typeof action !== 'string') return;
  if (action === 'web-reload') {
    if (webUrl) win.webview.loadURL(webUrl);
    return;
  }
  if (action === 'zoom-in' || action === 'zoom-out' || action === 'zoom-reset') {
    if (action === 'zoom-reset') {
      pageZoom = 1.0;
    } else {
      const step = action === 'zoom-in' ? 0.1 : -0.1;
      pageZoom = Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, Math.round((pageZoom + step) * 10) / 10));
    }
    win.webview.setPageZoom(pageZoom);
    console.log(`[lite] page zoom: ${pageZoom.toFixed(1)}x`);
  }
});

// Cleanup on GUI quit paths (Cmd+Q / window close), not just signals.
win.on("close", () => {
  cleanupOwnedDaemon(daemonPaths);
});
// Cmd+Q (App menu quit role) — approve the before-quit handshake, then exit
// through the same cleanup path as signals. Without a handler the role never
// terminates the bun process (the window stays alive).
electrobunEventEmitter.on("before-quit", (event: { response?: { allow?: boolean } }) => {
  cleanupOwnedDaemon(daemonPaths);
  if (event && typeof event === "object") {
    event.response = { allow: true };
  }
  setImmediate(() => process.exit(0));
});
process.on("SIGINT", () => {
  cleanupOwnedDaemon(daemonPaths);
  process.exit(0);
});
process.on("SIGTERM", () => {
  cleanupOwnedDaemon(daemonPaths);
  process.exit(0);
});

// src/bun/index.ts → worktree root is 4 levels up (src/bun → src → app → apps).
const daemon = await ensureDaemon(daemonPaths, {
  isPackaged: false,
  devRepoRoot: findRepoRoot(import.meta.dir),
});

let webUrl: string | undefined;
if (daemon) {
  // Server token in the `#token=` fragment — same as `mirri web` and the
  // Electron shell — so the web UI authenticates without the manual dialog.
  const token = readServerToken();
  const fragment = token === undefined ? '' : `#token=${encodeURIComponent(token)}`;
  console.log(`[lite] loading web UI from ${daemon.origin} (token: ${token === undefined ? 'absent — dialog fallback' : 'injected'})`);
  webUrl = `${daemon.origin}/?mirri_desktop=1&platform=darwin${fragment}`;
  win.webview.loadURL(webUrl);
} else {
  console.log("[lite] daemon unavailable — loading bundled self-check page");
}
