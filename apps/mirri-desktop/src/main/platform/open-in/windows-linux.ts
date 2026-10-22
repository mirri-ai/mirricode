// apps/mirri-desktop/src/main/platform/open-in/windows-linux.ts
//
// Windows / Linux implementation of the open-in launch dispatch: the file
// manager reveals via explorer.exe / xdg-open; editors and terminals fall
// back to opening the path with the OS default (xdg-open — including the
// win32 dead-end, deliberately preserved as-is: real Windows support is
// separate work).

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
    switch (platform) {
      case 'win32':
        return options.isDirectory
          ? { command: 'explorer.exe', args: [absolutePath] }
          : { command: 'explorer.exe', args: [`/select,${absolutePath}`] };
      default:
        return {
          command: 'xdg-open',
          args: [options.isDirectory ? absolutePath : path.dirname(absolutePath)],
        };
    }
  }
  // 非 macOS：没有编辑器/终端的平台专属语义 —— 用目录默认打开。
  return { command: 'xdg-open', args: [absolutePath] };
}
