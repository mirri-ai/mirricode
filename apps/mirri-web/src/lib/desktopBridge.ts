// apps/mirri-web/src/lib/desktopBridge.ts
//
// The renderer-side handle on the Mirri Desktop shell's preload bridge
// (`window.mirriDesktop`, exposed by apps/mirri-desktop/src/main/preload.ts).
//
// The interface mirrors `DesktopBridgeApi` in apps/mirri-desktop/src/main/
// ipc.ts — the two packages cannot import each other, so both definitions
// carry this note and must be kept in sync.
//
// Everything here is optional at runtime: in a plain browser (the SPA served
// directly by the daemon) `window.mirriDesktop` is undefined and
// `getDesktopBridge()` returns null — callers then degrade gracefully
// (browser Notification API, clipboard-copy prompts for file opens, ...).


export type { MirriDesktopBridge } from './desktopBridgeContract';

import type { MirriDesktopBridge } from './desktopBridgeContract';

declare global {
  interface Window {
    mirriDesktop?: MirriDesktopBridge;
  }
}

/** Members probed by the structural check — typed against the contract so a
 *  typo fails compile instead of silently admitting a broken bridge. */
const REQUIRED_BRIDGE_MEMBERS = [
  'openInEditor',
  'revealPath',
  'notify',
  'setBadgeCount',
  'listOpenInApps',
  'onDeepLink',
] as const satisfies ReadonlyArray<keyof MirriDesktopBridge>;

export { REQUIRED_BRIDGE_MEMBERS };

/** The desktop shell bridge, or null in a plain browser. The member check is
 *  structural: a stale bundle meeting a newer shell (or vice versa) degrades
 *  to null rather than half-working. */
export function getDesktopBridge(): MirriDesktopBridge | null {
  if (typeof window === 'undefined') return null;
  const bridge = window.mirriDesktop;
  if (bridge === undefined || bridge === null) return null;
  for (const member of REQUIRED_BRIDGE_MEMBERS) {
    if (typeof bridge[member] !== 'function') return null;
  }
  return bridge;
}

/**
 * Thrown by openFile/revealFile/openInApp when NO tier of the degrade chain
 * could open the target (no desktop bridge + no engine fs.open capability).
 * Carriers the best-effort display path so the UI can degrade to a friendly
 * "copy the path" prompt instead of a raw error dialog.
 */
export class ExternalOpenUnavailableError extends Error {
  readonly target: string;

  constructor(operation: 'openFile' | 'revealFile' | 'openInApp', target: string) {
    super(`WsRpcApi.${operation}: no opener available (browser session)`);
    this.name = 'ExternalOpenUnavailableError';
    this.target = target;
  }
}

/** Type guard for the degrade-chain sentinel error. */
export function isExternalOpenUnavailableError(err: unknown): err is ExternalOpenUnavailableError {
  return err instanceof ExternalOpenUnavailableError;
}

/**
 * Resolve a workspace-relative path against the session cwd into an absolute
 * path for the desktop shell (which opens files outside any session scope).
 * Absolute inputs pass through unchanged.
 */
export function resolveWorkspacePath(cwd: string, path: string): string {
  if (path.startsWith('/') || path.startsWith('\\')) return path;
  if (/^[A-Za-z]:[\\/]/.test(path)) return path; // Windows drive letter
  const trimmedCwd = cwd.endsWith('/') || cwd.endsWith('\\') ? cwd : `${cwd}/`;
  return `${trimmedCwd}${path}`;
}

/**
 * Parse a `mirricode://` deep link on the web side: returns the session id
 * for `mirricode://session/<id>`, or null for anything else (the shell has
 * already filtered to recognised shapes before forwarding, but the parse is
 * defensive — a malformed link must never break the handler).
 */
export function parseSessionDeepLink(url: string): string | null {
  if (typeof url !== 'string' || url.length === 0) return null;
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return null;
  }
  if (parsed.protocol !== 'mirricode:' || parsed.host !== 'session') return null;
  const raw = decodeURIComponent(parsed.pathname.replace(/^\//, ''));
  if (raw.length === 0 || raw.length > 256 || raw.includes('/') || raw.includes('\0')) {
    return null;
  }
  return raw;
}
