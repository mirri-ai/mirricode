// apps/mirri-desktop/src/main/platform/open-in/darwin.ts
//
// macOS implementation of the open-in launch dispatch: Finder reveal/open
// for the file manager, `open -a <Bundle>` for editors and terminals.

import type { OpenInAppDescriptor } from '../../open-in-apps';
import type { LaunchCommand, OpenInAppOptions } from './contract';

export function darwinLaunch(
  app: OpenInAppDescriptor,
  absolutePath: string,
  options: OpenInAppOptions,
): LaunchCommand {
  if (app.kind === 'file-manager') {
    return options.isDirectory
      ? { command: 'open', args: [absolutePath] }
      : { command: 'open', args: ['-R', absolutePath] };
  }
  // 编辑器 / 终端：macOS 用 `open -a <AppName>`。
  return { command: 'open', args: ['-a', app.macAppBundle!, absolutePath] };
}
