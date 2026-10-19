// apps/mirri-desktop/src/main/file-launch.ts
//
// Pure launch-command resolution for opening files from the renderer:
// `$MIRRICODE_EDITOR`/`$VISUAL`/`$EDITOR` for editor opens, and the
// open-in-app registry (open-in-apps.ts) for "open in app" launches.
//
// Adapted from the v1 TS daemon's fileLaunch module (packages/kap-server/src/
// lib/fileLaunch.ts) — the desktop shell owns the same responsibility now
// that the native open lives in Electron's main process instead of the
// daemon's HTTP handler.

import { spawn } from 'node:child_process';
import path from 'node:path';

import { OPEN_IN_APP_REGISTRY, type OpenInAppDescriptor } from './open-in-apps';

export interface LaunchCommand {
  readonly command: string;
  readonly args: readonly string[];
  readonly shell?: boolean;
}

export interface OpenInAppOptions {
  readonly line?: number;
  readonly isDirectory?: boolean;
}

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

function openInFinder(
  absolutePath: string,
  isDirectory: boolean | undefined,
  platform: NodeJS.Platform,
): LaunchCommand {
  switch (platform) {
    case 'darwin':
      return isDirectory
        ? { command: 'open', args: [absolutePath] }
        : { command: 'open', args: ['-R', absolutePath] };
    case 'win32':
      return isDirectory
        ? { command: 'explorer.exe', args: [absolutePath] }
        : { command: 'explorer.exe', args: [`/select,${absolutePath}`] };
    default:
      return {
        command: 'xdg-open',
        args: [isDirectory ? absolutePath : path.dirname(absolutePath)],
      };
  }
}

function openInMacApp(
  appName: string,
  absolutePath: string,
  platform: NodeJS.Platform,
): LaunchCommand {
  if (platform === 'darwin') {
    return { command: 'open', args: ['-a', appName, absolutePath] };
  }
  // Non-macOS: no iTerm/Terminal.app — open the directory with the OS default.
  return { command: 'xdg-open', args: [absolutePath] };
}

function registryApp(appId: string): OpenInAppDescriptor {
  const app = OPEN_IN_APP_REGISTRY.find((a) => a.appId === appId);
  if (app === undefined) throw new Error(`openInApp: unknown app id ${appId}`);
  return app;
}

/** Launch command for a registry app id. */
export function openInAppLaunch(
  appId: string,
  absolutePath: string,
  options: OpenInAppOptions,
  platform: NodeJS.Platform,
): LaunchCommand {
  const app = registryApp(appId);
  if (app.kind === 'file-manager') {
    return openInFinder(absolutePath, options.isDirectory, platform);
  }
  // 编辑器 / 终端：macOS 用 `open -a <AppName>`；其他平台沿用目录默认打开。
  return openInMacApp(app.macAppBundle!, absolutePath, platform);
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
