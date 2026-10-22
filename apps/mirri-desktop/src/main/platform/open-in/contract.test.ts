// apps/mirri-desktop/src/main/platform/open-in/contract.test.ts
//
// Behavior spec for the platform-layer open-in launch dispatch. Mirrors the
// behaviors previously asserted in file-launch.test.ts (which keeps running
// against the file-launch facade): explicit `isDirectory` inputs select the
// file-manager branches; production callers pass `{}` (undefined).

import { describe, expect, it } from 'vitest';

import { platformOpenInLaunch } from './contract';

describe('platform open-in launch dispatch', () => {
  it('should launch a registry app with open -a on darwin', () => {
    const cmd = platformOpenInLaunch('vscode', '/w/dir', {}, 'darwin');
    expect(cmd).toEqual({ command: 'open', args: ['-a', 'Visual Studio Code', '/w/dir'] });
  });

  it('should open a directory with explorer.exe on win32', () => {
    const cmd = platformOpenInLaunch('finder', '/w/dir', { isDirectory: true }, 'win32');
    expect(cmd).toEqual({ command: 'explorer.exe', args: ['/w/dir'] });
  });

  it('should fall back to xdg-open on linux for a directory', () => {
    const cmd = platformOpenInLaunch('finder', '/w/dir', { isDirectory: true }, 'linux');
    expect(cmd).toEqual({ command: 'xdg-open', args: ['/w/dir'] });
  });

  it('should throw for an unknown app id on any platform', () => {
    expect(() => platformOpenInLaunch('not-an-app', '/w', {}, 'darwin')).toThrow(/unknown app id/);
  });
});
