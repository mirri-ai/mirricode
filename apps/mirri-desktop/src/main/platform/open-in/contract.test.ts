// apps/mirri-desktop/src/main/platform/open-in/contract.test.ts
//
// Behavior spec for the platform-layer open-in launch dispatch. Mirrors the
// behaviors previously asserted in file-launch.test.ts (which keeps running
// against the file-launch facade): explicit `isDirectory` inputs select the
// file-manager branches, and the caller passes the resolved per-platform
// launch target (macOS bundle name / Windows exe).

import { describe, expect, it } from 'vitest';

import { platformOpenInLaunch } from './contract';

describe('platform open-in launch dispatch', () => {
  it('should launch a registry app with open -a on darwin', () => {
    const cmd = platformOpenInLaunch(
      'vscode',
      '/w/dir',
      { macAppBundleName: 'Visual Studio Code' },
      'darwin',
    );
    expect(cmd).toEqual({ command: 'open', args: ['-a', 'Visual Studio Code', '/w/dir'] });
  });

  it('should launch the installed Community Edition bundle on darwin', () => {
    const cmd = platformOpenInLaunch(
      'pycharm',
      '/w/dir',
      { macAppBundleName: 'PyCharm CE' },
      'darwin',
    );
    expect(cmd).toEqual({ command: 'open', args: ['-a', 'PyCharm CE', '/w/dir'] });
  });

  it('should throw when a darwin editor has no resolved bundle', () => {
    expect(() => platformOpenInLaunch('vscode', '/ws', {}, 'darwin')).toThrow(/not installed/);
  });

  it('should open a directory with explorer.exe on win32', () => {
    const cmd = platformOpenInLaunch('finder', '/w/dir', { isDirectory: true }, 'win32');
    expect(cmd).toEqual({ command: 'explorer.exe', args: ['/w/dir'] });
  });

  it('should open the containing folder when a file is targeted on win32', () => {
    // `/select,<path>` is mangled by Node's argument quoting, so the folder is
    // opened instead of the (broken) file selection.
    const cmd = platformOpenInLaunch(
      'finder',
      'C:\\my ws\\a.txt',
      { isDirectory: false },
      'win32',
    );
    expect(cmd).toEqual({ command: 'explorer.exe', args: ['C:\\my ws'] });
  });

  it('should open the workspace folder itself through File Explorer', () => {
    const cmd = platformOpenInLaunch('finder', 'C:\\my ws', { isDirectory: true }, 'win32');
    expect(cmd).toEqual({ command: 'explorer.exe', args: ['C:\\my ws'] });
  });

  it('should launch a Windows editor through its resolved exe', () => {
    const cmd = platformOpenInLaunch(
      'vscode',
      'C:\\ws',
      { windowsExe: 'C:\\Program Files\\Microsoft VS Code\\Code.exe' },
      'win32',
    );
    expect(cmd).toEqual({
      command: 'C:\\Program Files\\Microsoft VS Code\\Code.exe',
      args: ['C:\\ws'],
    });
  });

  it('should throw when a Windows editor has no resolved exe', () => {
    expect(() => platformOpenInLaunch('vscode', 'C:\\ws', {}, 'win32')).toThrow(/not installed/);
  });

  it('should fall back to xdg-open on linux for a directory', () => {
    const cmd = platformOpenInLaunch('finder', '/w/dir', { isDirectory: true }, 'linux');
    expect(cmd).toEqual({ command: 'xdg-open', args: ['/w/dir'] });
  });

  it('should throw for an unknown app id on any platform', () => {
    expect(() => platformOpenInLaunch('not-an-app', '/w', {}, 'darwin')).toThrow(/unknown app id/);
  });
});
