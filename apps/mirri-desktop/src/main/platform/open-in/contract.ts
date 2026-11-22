// apps/mirri-desktop/src/main/platform/open-in/contract.ts
//
// Platform-layer contract for "open in app" launch-command resolution: a
// pure function of (appId, path, options, platform) that picks the darwin or
// windows/linux implementation. Registry-driven — the editor-CLI path
// ($EDITOR etc.) and detached spawning stay in file-launch.ts.

import { OPEN_IN_APP_REGISTRY, type OpenInAppDescriptor } from '../../open-in-apps';
import { darwinLaunch } from './darwin';
import { windowsLinuxLaunch } from './windows-linux';

export interface LaunchCommand {
  readonly command: string;
  readonly args: readonly string[];
  readonly shell?: boolean;
  /** Windows only: hide the launched process's console window. Defaults to
   *  true for console-backed launches (a `.cmd` shim would otherwise flash a
   *  window). GUI apps must pass `false`: the flag reaches them as
   *  `STARTF_USESHOWWINDOW`/`SW_HIDE`, and Explorer honours it for the window
   *  it creates — the folder opened, but invisibly. */
  readonly windowsHide?: boolean;
}

export interface OpenInAppOptions {
  readonly line?: number;
  readonly isDirectory?: boolean;
  /** Resolved Windows executable for editor/terminal launches on win32 (see
   *  `resolveWindowsExe`). Ignored on other platforms. */
  readonly windowsExe?: string;
  /** Resolved macOS bundle name for editor/terminal launches on darwin (see
   *  `resolveMacBundleName`) — the edition actually installed. Ignored on
   *  other platforms. */
  readonly macAppBundleName?: string;
}

function registryApp(appId: string): OpenInAppDescriptor {
  const app = OPEN_IN_APP_REGISTRY.find((a) => a.appId === appId);
  if (app === undefined) throw new Error(`openInApp: unknown app id ${appId}`);
  return app;
}

/** Launch command for a registry app id on the given platform. */
export function platformOpenInLaunch(
  appId: string,
  absolutePath: string,
  options: OpenInAppOptions,
  platform: NodeJS.Platform,
): LaunchCommand {
  const app = registryApp(appId);
  return platform === 'darwin'
    ? darwinLaunch(app, absolutePath, options)
    : windowsLinuxLaunch(app, absolutePath, options, platform);
}
