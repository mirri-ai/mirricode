// apps/mirri-desktop-lite/src/bun/bridge.ts
//
// The 13-method bridge handlers. Validation philosophy identical to the
// Electron shell's ipc.ts: the webview is untrusted — every argument is
// whitelist-validated before any OS action. Degradation matrix per ADR-005:
// missing capabilities return honest false values, never fake success.

import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { spawn } from "node:child_process";
import { openPath, openExternal, showItemInFolder, showNotification } from "electrobun/main/utils";

import type { ElectrobunRPCConfig } from "electrobun/main/rpc";

import { readAppBundleIcon } from "./open-in/app-icon";
import { detectInstalledOpenInApps, OPEN_IN_APP_IDS } from "./open-in/open-in-apps";
import { platformOpenInLaunch } from "./open-in/platform/contract";

import type { MirriSchema } from "../shared/schema";

// ---- validation ---------------------------------------------------------------
// Same limits as the Electron shell's ipc.ts payload validation.

function validatePath(value: unknown): string | null {
  if (typeof value !== "string") return null;
  const trimmed = value.trim();
  if (trimmed.length === 0 || trimmed.length > 4096) return null;
  if (trimmed.includes("\0")) return null;
  return trimmed;
}

function validateLine(value: unknown): number | undefined {
  if (value === undefined || value === null) return undefined;
  if (typeof value !== "number" || !Number.isInteger(value) || value < 1 || value > 10_000_000) return undefined;
  return value;
}

function validateHttpUrl(value: unknown): string | null {
  if (typeof value !== "string") return null;
  try {
    const u = new URL(value);
    if (u.protocol !== "http:" && u.protocol !== "https:") return null;
    return value;
  } catch {
    return null;
  }
}

/** Bounded string for notification fields (main shell: title ≤200, body ≤500). */
function validateText(value: unknown, max: number): string | null {
  if (typeof value !== "string") return null;
  const trimmed = value.trim();
  if (trimmed.length === 0 || trimmed.length > max) return null;
  return trimmed;
}

function invalid(kind: string): never {
  throw new Error(`invalid ${kind}`);
}

// ---- app-icon + detection deps -------------------------------------------------

/** Detect installed open-in apps with real bundle icons (same shape the
 *  Electron shell passes in register-ipc.ts). */
const detectDeps = {
  platform: process.platform,
  existsSync,
  applicationsDirs: ['/Applications', join(homedir(), 'Applications')],
  getAppBundleIcon: (bundlePath: string) => readAppBundleIcon(bundlePath),
};

// ---- launch helpers (ported shapes of the shell's file-launch.ts) ----------------

/** `$MIRRICODE_EDITOR` → `$VISUAL` → `$EDITOR`, or null when unset. */
function resolveEditorCommand(): string | null {
  for (const key of ['MIRRICODE_EDITOR', 'VISUAL', 'EDITOR']) {
    const value = process.env[key];
    if (typeof value === 'string' && value.trim().length > 0) return value.trim();
  }
  return null;
}

/** True when the editor accepts a `path:line` target (VS Code-likes). */
function supportsLineTarget(command: string): boolean {
  const first = command.trim().split(/\s+/)[0] ?? '';
  return /(?:^|\/)(code|cursor|windsurf)(?:\.cmd|\.exe)?$/i.test(first);
}

function quoteShellArg(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

/** Open in the user's editor (path:line for VS Code-likes), falling back to
 *  the OS default app when no editor is configured. */
function openInEditor(path: string, line: number | undefined): void {
  const editor = resolveEditorCommand();
  if (!editor) {
    openPath(path);
    return;
  }
  const target = supportsLineTarget(editor) && line !== undefined ? `${path}:${line}` : path;
  const child = spawn(`${editor} ${quoteShellArg(target)}`, { shell: true, detached: true, stdio: 'ignore' });
  child.once('error', (err) => {
    console.log(`[lite] editor ${editor} failed (${err.message}) — falling back to OS default`);
    openPath(path);
  });
  child.unref();
}

/** Registry-driven launch (port of the shell's file-launch.ts spawning). */
function launchOpenInApp(appId: string, path: string): void {
  const cmd = platformOpenInLaunch(appId, path, {}, process.platform);
  const child = spawn(cmd.command, [...cmd.args], { detached: true, stdio: 'ignore', shell: cmd.shell === true });
  child.once('error', (err) => console.error(`[lite] open-in launch failed: ${err.message}`));
  child.unref();
}

// ---- the RPC config --------------------------------------------------------------

/** The ElectrobunRPCConfig for the lite shell's bridge. */
export function createBridgeHandlers(): ElectrobunRPCConfig<MirriSchema, 'bun'> {
  return {
    handlers: {
      requests: {
        openPath: ({ path }) => {
          const p = validatePath(path) ?? invalid('path');
          openPath(p);
          return { opened: true };
        },
        revealPath: ({ path }) => {
          const p = validatePath(path) ?? invalid('path');
          showItemInFolder(p);
          return { revealed: true };
        },
        openInEditor: ({ path, line }) => {
          const p = validatePath(path) ?? invalid('path');
          openInEditor(p, validateLine(line));
          return { opened: true };
        },
        openInApp: ({ appId, path }) => {
          if (typeof appId !== 'string' || !OPEN_IN_APP_IDS.includes(appId)) invalid('app id');
          const p = validatePath(path) ?? invalid('path');
          launchOpenInApp(appId, p);
          return { opened: true };
        },
        listOpenInApps: async () => {
          console.log('[lite] listOpenInApps — registry detection + icons');
          return detectInstalledOpenInApps(detectDeps);
        },
        openExternal: ({ url }) => {
          const u = validateHttpUrl(url) ?? invalid('url');
          openExternal(u);
          return { opened: true };
        },
        notify: ({ title, body }) => {
          const t = validateText(title, 200) ?? invalid('title');
          const b = validateText(body, 500) ?? invalid('body');
          showNotification({ title: t, body: b });
          return { shown: true };
        },
        // ---- degraded capabilities (ADR-005: honest false, never fake success) ----
        setBadgeCount: () => {
          console.log('[lite] setBadgeCount — no API: degraded (returns {set:false})');
          return { set: false };
        },
        dockBounce: () => {
          // The contract returns void; there is nothing observable to fake.
          // Honest no-op with an explicit degraded log.
          console.log('[lite] dockBounce — no API: degraded no-op');
          return { bounced: false };
        },
        setLoginItem: () => {
          console.log('[lite] setLoginItem — no API: degraded (write does not persist)');
          return { open: false };
        },
        getLoginItem: () => {
          return { open: false };
        },
      },
    },
  };
}
