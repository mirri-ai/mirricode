// apps/mirri-desktop/src/main/platform/index.ts
//
// The platform layer: the ONLY place that reads process.platform. Feature
// modules and the composition root ask this module instead, so OS-specific
// behavior has a single home and tests can inject platforms.

import { platformOpenInLaunch } from './open-in/contract';

export { platformOpenInLaunch };
export type { LaunchCommand, OpenInAppOptions } from './open-in/contract';

export function currentPlatform(): NodeJS.Platform {
  return process.platform;
}

export function isDarwin(): boolean {
  return process.platform === 'darwin';
}

/** macOS tray slots are 16 points (32px retina); other platforms 16px. */
export function trayIconSize(): 32 | 16 {
  return process.platform === 'darwin' ? 32 : 16;
}
