// apps/mirri-desktop/src/main/deep-link.ts
//
// `mirricode://` deep-link parsing — pure, no electron import. The wiring in
// index.ts feeds URLs from three sources into the same parser:
//   - macOS `open-url` events (app already running),
//   - the second-instance argv (Windows/Linux, app already running),
//   - the initial process.argv (cold start launched by the OS).
//
// Recognised shapes (only one feature today — session deep links; 'open' is
// the bare "focus the window" action):
//   mirricode://open             → focus
//   mirricode://session/<id>     → focus + switch the web UI to that session

const SCHEME = 'mirricode:';
const MAX_SESSION_ID_LENGTH = 256;

export type DeepLinkAction = { type: 'open' } | { type: 'session'; sessionId: string };

/** Parse a deep-link URL. Returns null for anything unrecognised (never
 *  throws — a malformed link must never crash the shell). */
export function parseDeepLink(url: string): DeepLinkAction | null {
  if (typeof url !== 'string' || url.length === 0) return null;
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return null;
  }
  if (parsed.protocol !== SCHEME) return null;

  // 'mirricode://open' has host 'open'; a bare 'mirricode://' has an empty
  // host. Both mean "bring the app to the front". Opaque-path forms
  // ('mirricode:session/x') are NOT accepted — only the host-based shape.
  if (parsed.host === 'open' && parsed.pathname === '') return { type: 'open' };
  if (parsed.host === '' && (parsed.pathname === '' || parsed.pathname === '/')) {
    return { type: 'open' };
  }

  if (parsed.host === 'session') {
    const raw = decodeURIComponent(parsed.pathname.replace(/^\//, ''));
    if (raw.length === 0 || raw.length > MAX_SESSION_ID_LENGTH) return null;
    if (raw.includes('/') || raw.includes('\0')) return null;
    return { type: 'session', sessionId: raw };
  }

  return null;
}

/** Pick the mirricode:// entries out of a process argv. */
export function extractDeepLinks(argv: readonly string[]): string[] {
  return argv.filter((arg) => arg.startsWith(`${SCHEME}//`));
}
