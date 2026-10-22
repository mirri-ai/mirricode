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
}

export interface OpenInAppOptions {
  readonly line?: number;
  readonly isDirectory?: boolean;
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
