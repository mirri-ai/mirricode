import { describe, expect, it } from 'vitest';

import {
  editorLaunchFor,
  openInAppLaunch,
  resolveEditorCommand,
  supportsLineTarget,
} from './file-launch';

const MAC = 'darwin';
const LINUX = 'linux';

describe('resolveEditorCommand', () => {
  it('should prefer MIRRICODE_EDITOR over VISUAL/EDITOR', () => {
    expect(
      resolveEditorCommand({ MIRRICODE_EDITOR: 'code', VISUAL: 'vim', EDITOR: 'nano' }),
    ).toBe('code');
  });

  it('should prefer VISUAL over EDITOR', () => {
    expect(resolveEditorCommand({ VISUAL: 'vim', EDITOR: 'nano' })).toBe('vim');
  });

  it('should fall back to EDITOR', () => {
    expect(resolveEditorCommand({ EDITOR: 'nano' })).toBe('nano');
  });

  it('should trim whitespace and skip empty values', () => {
    expect(resolveEditorCommand({ VISUAL: '  ', EDITOR: ' nano ' })).toBe('nano');
  });

  it('should return null when nothing is set', () => {
    expect(resolveEditorCommand({})).toBeNull();
  });
});

describe('supportsLineTarget', () => {
  it('should accept vscode-like editors', () => {
    expect(supportsLineTarget('code')).toBe(true);
    expect(supportsLineTarget('cursor')).toBe(true);
    expect(supportsLineTarget('/usr/local/bin/code')).toBe(true);
    expect(supportsLineTarget('code.cmd')).toBe(true);
  });

  it('should reject editors without a line target syntax', () => {
    expect(supportsLineTarget('vim')).toBe(false);
    expect(supportsLineTarget('nano')).toBe(false);
  });
});

describe('editorLaunchFor', () => {
  it('should build a shell launch with the editor and quoted path', () => {
    const launch = editorLaunchFor('/tmp/my file.rs', undefined, { EDITOR: 'vim' }, MAC);
    expect(launch).toEqual({ command: "vim '/tmp/my file.rs'", args: [], shell: true });
  });

  it('should append :line for editors that support it', () => {
    const launch = editorLaunchFor('/tmp/a.rs', 12, { EDITOR: 'code' }, MAC);
    expect(launch).toEqual({ command: "code '/tmp/a.rs:12'", args: [], shell: true });
  });

  it('should keep multi-word editor commands intact', () => {
    const launch = editorLaunchFor('/tmp/a.rs', undefined, { EDITOR: 'code --wait' }, MAC);
    expect(launch).toEqual({ command: "code --wait '/tmp/a.rs'", args: [], shell: true });
  });

  it('should quote for cmd on win32', () => {
    const launch = editorLaunchFor('C:\\a b.rs', undefined, { EDITOR: 'code' }, 'win32');
    expect(launch).toEqual({ command: 'code "C:\\a b.rs"', args: [], shell: true });
  });

  it('should return null when no editor is configured', () => {
    expect(editorLaunchFor('/tmp/a.rs', 3, {}, MAC)).toBeNull();
  });
});

describe('openInAppLaunch', () => {
  it('should reveal a file in Finder on macOS', () => {
    expect(openInAppLaunch('finder', '/tmp/a', { isDirectory: false }, MAC)).toEqual({
      command: 'open',
      args: ['-R', '/tmp/a']
    });
  });

  it('should open a directory in Finder on macOS', () => {
    expect(openInAppLaunch('finder', '/tmp', { isDirectory: true }, MAC)).toEqual({
      command: 'open',
      args: ['/tmp']
    });
  });

  it('should select in Explorer on win32', () => {
    expect(openInAppLaunch('finder', 'C:\\ws', { isDirectory: false }, 'win32')).toEqual({
      command: 'explorer.exe',
      args: ['/select,C:\\ws']
    });
  });

  it('should open the parent directory on linux', () => {
    expect(openInAppLaunch('finder', '/tmp/a/b', { isDirectory: false }, LINUX)).toEqual({
      command: 'xdg-open',
      args: ['/tmp/a']
    });
  });

  it('should launch vscode via open -a on macOS', () => {
    expect(openInAppLaunch('vscode', '/ws', { line: 4 }, MAC)).toEqual({
      command: 'open',
      args: ['-a', 'Visual Studio Code', '/ws'],
    });
  });

  it('should launch vscode without a line when none given', () => {
    expect(openInAppLaunch('vscode', '/ws', {}, MAC)).toEqual({
      command: 'open',
      args: ['-a', 'Visual Studio Code', '/ws'],
    });
  });

  it('should launch cursor via open -a on macOS', () => {
    expect(openInAppLaunch('cursor', '/ws', { line: 2 }, MAC)).toEqual({
      command: 'open',
      args: ['-a', 'Cursor', '/ws'],
    });
  });

  it('should open Terminal/iTerm via open -a on macOS', () => {
    expect(openInAppLaunch('terminal', '/ws', {}, MAC)).toEqual({
      command: 'open',
      args: ['-a', 'Terminal', '/ws']
    });
    expect(openInAppLaunch('iterm', '/ws', {}, MAC)).toEqual({
      command: 'open',
      args: ['-a', 'iTerm', '/ws']
    });
  });

  it('should launch via open -a when the app id is a registry terminal', () => {
    const cmd = openInAppLaunch('warp', '/work/project', {}, 'darwin');
    expect(cmd).toEqual({ command: 'open', args: ['-a', 'Warp', '/work/project'] });
  });

  it('should launch via open -a when the app id is a registry editor', () => {
    const cmd = openInAppLaunch('zed', '/work/project', {}, 'darwin');
    expect(cmd).toEqual({ command: 'open', args: ['-a', 'Zed', '/work/project'] });
  });

  it('should throw when the app id is not in the registry', () => {
    expect(() => openInAppLaunch('rm-rf', '/work', {}, 'darwin')).toThrow();
  });
});
