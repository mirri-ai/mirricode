// apps/mirri-desktop/src/main/file-launch.ts
//
// Pure launch-command resolution for opening files from the renderer:
// `$MIRRICODE_EDITOR`/`$VISUAL`/`$EDITOR` for editor opens, and (as a thin
// facade) the platform layer (platform/open-in/) for "open in app" launches.
//
// Adapted from the v1 TS daemon's fileLaunch module (packages/kap-server/src/
// lib/fileLaunch.ts) — the desktop shell owns the same responsibility now
// that the native open lives in Electron's main process instead of the
// daemon's HTTP handler.

import { spawn } from 'node:child_process';

import type { LaunchCommand, OpenInAppOptions } from './platform';
import { platformOpenInLaunch } from './platform';

export type { LaunchCommand } from './platform';

/** `$MIRRICODE_EDITOR` → `$VISUAL` → `$EDITOR`, or null when unset. */
export function resolveEditorCommand(
  env: Record<string, string | undefined>,
): string | null {
  for (const key of ['MIRRICODE_EDITOR', 'VISUAL', 'EDITOR']) {
    const value = env[key];
    if (typeof value === 'string' && value.trim().length > 0) return value.trim();
  }
  return null;
}

/** True when the editor accepts a `path:line` target (VS Code-likes). */
export function supportsLineTarget(command: string): boolean {
  const first = command.trim().split(/\s+/)[0] ?? '';
  return /(?:^|\/)(code|cursor|windsurf)(?:\.cmd|\.exe)?$/i.test(first);
}

function quoteShellArg(value: string, platform: NodeJS.Platform): string {
  if (platform === 'win32') return `"${value.replaceAll('"', '\\"')}"`;
  return `'${value.replaceAll("'", "'\\''")}'`;
}

/** Launch command that opens `absolutePath` in the user's editor, or null when
 *  no editor is configured (the caller then falls back to the OS default via
 *  `shell.openPath`). */
export function editorLaunchFor(
  absolutePath: string,
  line: number | undefined,
  env: Record<string, string | undefined>,
  platform: NodeJS.Platform,
): LaunchCommand | null {
  const editor = resolveEditorCommand(env);
  if (editor === null) return null;
  const target =
    supportsLineTarget(editor) && line !== undefined
      ? `${absolutePath}:${line}`
      : absolutePath;
  return {
    command: `${editor} ${quoteShellArg(target, platform)}`,
    args: [],
    shell: true,
  };
}

/** Launch command for a registry app id. */
export function openInAppLaunch(
  appId: string,
  absolutePath: string,
  options: OpenInAppOptions,
  platform: NodeJS.Platform,
): LaunchCommand {
  return platformOpenInLaunch(appId, absolutePath, options, platform);
}

/** Spawn a launch command detached from the shell's lifetime. Resolves when
 *  the process actually spawned; rejects when the binary is missing. */
export function launchDetached(cmd: LaunchCommand): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    let settled = false;
    const child = spawn(cmd.command, [...cmd.args], {
      detached: true,
      stdio: 'ignore',
      shell: cmd.shell === true,
    });
    child.once('error', (err) => {
      if (settled) return;
      settled = true;
      reject(err);
    });
    child.once('spawn', () => {
      if (settled) return;
      settled = true;
      child.unref();
      resolve();
    });
  });
}
