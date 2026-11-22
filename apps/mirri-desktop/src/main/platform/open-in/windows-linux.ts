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
    return { command: options.windowsExe, args: [absolutePath] };
  }
  // 非 macOS：没有编辑器/终端的平台专属语义 —— 用目录默认打开。
  return { command: 'xdg-open', args: [absolutePath] };
}
