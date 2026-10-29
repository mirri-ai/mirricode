// apps/mirri-desktop-lite/src/bun/token.ts
//
// Read the daemon's persistent bearer token (port of the Electron shell's
// server/token.ts) so the web UI authenticates without the manual token
// dialog on a fresh launch. The daemon's token file is shared across
// instances (load_or_create reads the same <mirri-home>/server.token), so
// the lite daemon on its own port accepts the same token.

import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

/** `<MIRRICODE_HOME>` or `~/.mirri-code` — must match the daemon's resolution. */
function mirriHome(): string {
  const override = process.env.MIRRICODE_HOME;
  if (override !== undefined && override.trim().length > 0) {
    return override;
  }
  return join(homedir(), '.mirri-code');
}

/**
 * Read the daemon's bearer token so the web UI can authenticate without
 * showing the manual token dialog on a fresh launch. Returns undefined when
 * the token cannot be read (the web UI then falls back to the dialog).
 */
export function readServerToken(): string | undefined {
  try {
    const token = readFileSync(join(mirriHome(), 'server.token'), 'utf-8').trim();
    return token.length > 0 ? token : undefined;
  } catch {
    return undefined;
  }
}
