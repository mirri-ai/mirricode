import { readFileSync } from 'node:fs';
import { join } from 'node:path';

import { mirriHome } from './state';

/** On-disk filename of the daemon's persistent bearer token (under MIRRICODE_HOME). */
const SERVER_TOKEN_FILE = 'server.token';

/**
 * Read the daemon's bearer token so the web UI can authenticate without showing
 * the manual token dialog on a fresh launch. Returns undefined when the token
 * cannot be read (the web UI then falls back to the dialog).
 */
export function readServerToken(): string | undefined {
  try {
    const token = readFileSync(join(mirriHome(), SERVER_TOKEN_FILE), 'utf-8').trim();
    return token.length > 0 ? token : undefined;
  } catch {
    return undefined;
  }
}
