// apps/mirri-desktop/src/main/platform/open-in/windows-linux.ts
//
// Windows / Linux implementation of the open-in launch dispatch: the file
// manager opens the folder via explorer.exe / xdg-open, editors launch their
// resolved Windows executable, and Linux editors/terminals fall back to
// opening the path with the OS default (xdg-open).
//
// Path splitting uses the explicit `path.win32` / `path.posix` flavor rather
// than the host default: this module is dispatched by the `platform` argument,
// so a Windows path must split the same way when the tests run on Linux CI.

import path from 'node:path';

import type { OpenInAppDescriptor } from '../../open-in-apps';
import type { LaunchCommand, OpenInAppOptions } from './contract';

/** Qualify a rooted-but-driveless Windows path with the drive of a reference
 *  path: `/Users/me/proj` → `C:\Users\me\proj`.
 *
 *  Windows itself resolves such a path against the *current* drive, and the
 *  current directory of an Electron main process is not the user's home (it is
 *  the install directory or `C:\Windows\System32`). Session/workspace records
 *  produced by the old path-resolution bug carry exactly this shape — the drive
 *  was dropped and `/Users/me/proj` survived — and no Windows API can open that
 *  as the user intended. The reference is the home/profile path, whose drive is
 *  where user workspaces live; the caller still verifies existence afterwards,
 *  so a wrong guess fails loudly instead of opening something unrelated.
 *
 *  Anything already drive-qualified (`C:\...`), UNC (`\\server\share`), or
 *  POSIX is returned unchanged. */
export function qualifyWindowsDrivePath(absolutePath: string, referencePath: string): string {
  const windowsPath = absolutePath.replaceAll('/', '\\');
  if (!windowsPath.startsWith('\\')) return absolutePath;
  if (windowsPath.startsWith('\\\\')) return absolutePath; // UNC — already qualified
  const drive = /^([A-Za-z]):/.exec(referencePath.replaceAll('/', '\\')); // e.g. `C:`
  if (drive === null) return absolutePath;
  return `${drive[1]}:${windowsPath}`;
}

export function windowsLinuxLaunch(
  app: OpenInAppDescriptor,
  absolutePath: string,
  options: OpenInAppOptions,
  platform: NodeJS.Platform,
): LaunchCommand {
  if (app.kind === 'file-manager') {
    if (platform === 'win32') {
      // explorer.exe takes the folder to open as its only argument. Its
      // `/select,<path>` form is unusable from spawn: Node quotes an argument
      // containing spaces as a whole (`"/select,C:\my dir"`) and explorer reads
      // that literally, so a file target reveals the containing folder instead.
      return {
        command: 'explorer.exe',
        args: [
          options.isDirectory ? absolutePath : path.win32.dirname(absolutePath),
        ],
        // explorer.exe is a GUI app and forwards to the running shell, which
        // applies the inherited show state to the window it creates — with
        // `windowsHide` the folder opens perfectly but the window stays
        // invisible, which reads as "the click did nothing".
        windowsHide: false,
      };
    }
    return {
      command: 'xdg-open',
      args: [
        options.isDirectory ? absolutePath : path.posix.dirname(absolutePath),
      ],
    };
  }
  if (platform === 'win32') {
    // Editors: launch the installed exe directly (no shell — a `.cmd` shim
    // would flash a console window). detection and this resolver share
    // resolveWindowsExe, so a missing exe means the app was uninstalled after
    // the dropdown was populated.
    if (options.windowsExe === undefined) {
      throw new Error(`openInApp: ${app.appId} is not installed on this machine`);
    }
    // Editors are GUI apps too; hide only their console window, never the
    // window they create.
    return { command: options.windowsExe, args: [absolutePath], windowsHide: false };
  }
  // 非 macOS：没有编辑器/终端的平台专属语义 —— 用目录默认打开。
  return { command: 'xdg-open', args: [absolutePath] };
}
