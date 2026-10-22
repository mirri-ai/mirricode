// apps/mirri-desktop/src/main/ipc.ts
//
// IPC surface shared by the preload bridge and the main process. Channel
// names + payload validation, pure of any `electron` import so the protocol
// is unit-testable (the preload inlines this module at bundle time; the main
// process registers the handlers in register-ipc.ts).
//
// Threat model: the renderer is the (untrusted) web UI served over the local
// daemon connection, so every payload from `ipcRenderer.invoke` is validated
// here before it reaches `shell.*` / `spawn` / OS notification surfaces.

/** App ids the shell may launch on behalf of the renderer. Single source of
 *  truth is the open-in-app registry (open-in-apps.ts) — the renderer cannot
 *  ask for an arbitrary binary, but may name any registry member.
 *  Re-exported here as the IPC-facing allowlist. */
import { OPEN_IN_APP_IDS, type OpenInAppId, type DetectedOpenInApp } from './open-in-apps';
export { OPEN_IN_APP_IDS };
export type { OpenInAppId };

/** Every channel the desktop shell exposes. Namespaced `desktop:` so a
 *  future multiplexed bridge cannot collide with engine channels. */
export const IPC_CHANNELS = {
  openPath: 'desktop:open-path',
  revealPath: 'desktop:reveal-path',
  openInEditor: 'desktop:open-in-editor',
  openInApp: 'desktop:open-in-app',
  listOpenInApps: 'desktop:list-open-in-apps',
  openExternal: 'desktop:open-external',
  notify: 'desktop:notify',
  setBadgeCount: 'desktop:set-badge-count',
  dockBounce: 'desktop:dock-bounce',
  setLoginItem: 'desktop:set-login-item',
  getLoginItem: 'desktop:get-login-item',
  // Main → renderer events (webContents.send, not invoke).
  deepLink: 'desktop:deep-link',
  notificationClick: 'desktop:notification-click',
} as const;

export type IpcChannel = (typeof IPC_CHANNELS)[keyof typeof IPC_CHANNELS];

/**
 * The renderer-facing bridge exposed as `window.mirriDesktop` by the preload
 * script. The web UI (apps/mirri-web/src/lib/desktopBridgeContract.ts) owns
 * the contract; this interface implements it. bridge-contract.test.ts
 * enforces the match at compile time — a drift on either side fails desktop
 * typecheck.
 *
 * Handlers are invoke-style (Promise) except the two main→renderer events,
 * which use callback registration.
 */
export interface DesktopBridgeApi {
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
  listOpenInApps(): Promise<DetectedOpenInApp[]>;
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

const MAX_PATH_LENGTH = 4096;
const MAX_TITLE_LENGTH = 200;
const MAX_BODY_LENGTH = 500;
const MAX_TAG_LENGTH = 200;

export interface PathPayload {
  path: string;
  line?: number;
}

function isPlainObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null;
}

/** A local filesystem path the renderer wants opened/revealed. Non-empty,
 *  NUL-free, length-capped; otherwise unconstrained (any file on the user's
 *  machine is fair game for a local shell). */
function validatePath(value: unknown): string | null {
  if (typeof value !== 'string') return null;
  const path = value.trim();
  if (path.length === 0 || path.length > MAX_PATH_LENGTH) return null;
  if (path.includes('\0')) return null;
  return path;
}

/** `{ path, line? }` for editor opens. */
export function validatePathPayload(payload: unknown): PathPayload | null {
  if (!isPlainObject(payload)) return null;
  const path = validatePath(payload['path']);
  if (path === null) return null;
  let line: number | undefined;
  if (payload['line'] !== undefined) {
    const raw = payload['line'];
    if (typeof raw !== 'number' || !Number.isSafeInteger(raw) || raw < 1) return null;
    line = raw;
  }
  return { path, line };
}

/** `{ appId, path }` for "open workspace in app" launches. */
export function validateOpenInAppPayload(payload: unknown): { appId: OpenInAppId; path: string } | null {
  if (!isPlainObject(payload)) return null;
  const appId = payload['appId'];
  if (typeof appId !== 'string' || !OPEN_IN_APP_IDS.includes(appId)) {
    return null;
  }
  const path = validatePath(payload['path']);
  if (path === null) return null;
  return { appId, path };
}

/** `{ url }` for external-browser opens — http(s) only (no file:, no custom
 *  schemes; the web UI links out to docs/repos, nothing else). */
export function validateExternalUrlPayload(payload: unknown): string | null {
  if (!isPlainObject(payload)) return null;
  const url = payload['url'];
  if (typeof url !== 'string') return null;
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return null;
  }
  if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') return null;
  return parsed.toString();
}

/** `{ title, body, tag? }` for native notifications. */
export function validateNotifyPayload(
  payload: unknown,
): { title: string; body: string; tag: string | undefined } | null {
  if (!isPlainObject(payload)) return null;
  const { title, body, tag } = payload;
  if (typeof title !== 'string' || title.trim().length === 0 || title.length > MAX_TITLE_LENGTH) {
    return null;
  }
  if (typeof body !== 'string' || body.length > MAX_BODY_LENGTH) return null;
  if (tag !== undefined) {
    if (typeof tag !== 'string' || tag.length === 0 || tag.length > MAX_TAG_LENGTH) return null;
  }
  return { title, body, tag: tag as string | undefined };
}

/** `{ count }` for the dock/taskbar badge — non-negative integer. */
export function validateBadgeCountPayload(payload: unknown): number | null {
  if (!isPlainObject(payload)) return null;
  const count = payload['count'];
  if (typeof count !== 'number' || !Number.isSafeInteger(count) || count < 0) return null;
  return count;
}

/** `{ open }` for login-item toggles. */
export function validateLoginItemPayload(payload: unknown): boolean | null {
  if (!isPlainObject(payload)) return null;
  const open = payload['open'];
  if (typeof open !== 'boolean') return null;
  return open;
}

/** Main → renderer deep-link forward: only `mirricode://` urls pass. */
export function validateDeepLinkForwardPayload(value: unknown): string | null {
  if (typeof value !== 'string' || value.length === 0) return null;
  let parsed: URL;
  try {
    parsed = new URL(value);
  } catch {
    return null;
  }
  if (parsed.protocol !== 'mirricode:') return null;
  return value;
}
