// apps/mirri-desktop/src/main/external-links.ts
// Pure helpers that decide how the Desktop BrowserWindow should handle link
// clicks and navigations. Extracted from index.ts so they can be unit-tested
// without an Electron runtime.

/** True only for http(s) URLs — the only protocols we forward to the system
 *  browser via `shell.openExternal`. Guards against `file://`, `javascript:`,
 *  `data:`, etc. */
export function isExternalHttpUrl(url: string): boolean {
  return /^https?:\/\//i.test(url);
}

/** Result of deciding what to do with a `target="_blank"` / `window.open`
 *  request. */
export interface WindowOpenDecision {
  /** Always `deny` — we never open an in-app Electron window. */
  action: 'deny';
  /** True when the URL should be forwarded to the system browser. */
  openExternally: boolean;
}

/** Decide what to do when the renderer asks to open a new window
 *  (`target="_blank"` link or `window.open()` call). */
export function decideWindowOpen(url: string): WindowOpenDecision {
  return { action: 'deny', openExternally: isExternalHttpUrl(url) };
}

/** Result of deciding what to do when the main window is about to navigate. */
export interface WillNavigateDecision {
  /** True when the navigation should be cancelled (it would leave the web UI). */
  preventDefault: boolean;
  /** True when the URL should be forwarded to the system browser. */
  openExternally: boolean;
}

/** Decide what to do when the main window is about to navigate to `url`.
 *  `currentOrigin` is the origin of the page currently loaded in the window. */
export function decideWillNavigate(url: string, currentOrigin: string): WillNavigateDecision {
  let targetOrigin: string;
  try {
    targetOrigin = new URL(url).origin;
  } catch {
    // Not a valid URL — block it to be safe, don't open externally.
    return { preventDefault: true, openExternally: false };
  }
  if (targetOrigin === currentOrigin) {
    // Same-origin navigation — allow (web UI internal routing).
    return { preventDefault: false, openExternally: false };
  }
  // External navigation — block and forward to system browser if http(s).
  return { preventDefault: true, openExternally: isExternalHttpUrl(url) };
}
