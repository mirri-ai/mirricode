// apps/mirri-web/src/lib/desktopBridgeContract.ts
//
// The `window.mirriDesktop` bridge contract — the single source of truth for
// what any desktop shell (Electron today, potentially others) must expose to
// the renderer. DOM-free by construction: the desktop package's contract test
// (apps/mirri-desktop/src/main/bridge-contract.test.ts) imports this file
// type-only under a no-DOM tsconfig, so nothing in here may reference
// window/document/navigator. The runtime accessor + structural degradation
// check live in desktopBridge.ts.

/**
 * The desktop shell bridge. All methods are invoke-style promises; the two
 * `on*` callbacks receive main→renderer events.
 */
export interface MirriDesktopBridge {
  /** Open a file/directory with the OS default app. Rejects on failure. */
  openPath(path: string): Promise<{ opened: true }>;
  /** Reveal a file in Finder / Explorer / the file manager. */
  revealPath(path: string): Promise<{ revealed: true }>;
  /** Open a file in the user's editor ($MIRRICODE_EDITOR/$VISUAL/$EDITOR),
   *  falling back to the OS default app. */
  openInEditor(path: string, line?: number): Promise<{ opened: true }>;
  /** Open a path in a fixed allowlisted app (finder/vscode/cursor/...). */
  openInApp(appId: string, path: string): Promise<{ opened: true }>;
  /** List installed "open in" target apps (detection + extracted logos). */
  listOpenInApps(): Promise<Array<{ appId: string; name: string; iconDataUrl?: string }>>;
  /** Open an http(s) URL in the system browser. */
  openExternal(url: string): Promise<{ opened: true }>;
  /** Show a native (main-process) notification; resolves `{shown:false}`
   *  when the platform can't show one (caller falls back to the browser API). */
  notify(title: string, body: string, tag?: string): Promise<{ shown: boolean }>;
  /** Set the dock/taskbar badge; 0 clears it. */
  setBadgeCount(count: number): Promise<{ set: boolean }>;
  /** Bounce the macOS dock icon (no-op elsewhere). */
  dockBounce(kind?: 'informational' | 'critical'): Promise<void>;
  /** Toggle launch-at-login. */
  setLoginItem(open: boolean): Promise<{ open: boolean }>;
  /** Read the current launch-at-login state. */
  getLoginItem(): Promise<{ open: boolean }>;
  /** Main → renderer: a `mirricode://` deep link arrived. */
  onDeepLink(callback: (url: string) => void): void;
  /** Main → renderer: the user clicked a native notification identified by
   *  its tag (the same tag passed to notify). */
  onNotificationClick(callback: (tag: string) => void): void;
}
