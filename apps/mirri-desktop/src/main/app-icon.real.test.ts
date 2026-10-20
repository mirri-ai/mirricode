// apps/mirri-desktop/src/main/app-icon.real.test.ts
//
// Real-world sanity check for the bundle-icon path: exercise the actual
// extraction against REAL bundles in /Applications. This validates the whole
// premise of the fix — that per-app icns extraction yields DIFFERENT, real
// icons where Electron's app.getFileIcon('.app') returned one shared
// placeholder. Skipped on non-macOS machines / when the bundles are absent
// (CI and Linux dev boxes never fail on this file).

import { existsSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import { readAppBundleIcon } from './app-icon';

const hasRealBundles =
  process.platform === 'darwin' &&
  existsSync('/Applications/Visual Studio Code.app') &&
  existsSync('/Applications/Zed.app');

describe('readAppBundleIcon (real bundles)', () => {
  it.skipIf(!hasRealBundles)(
    'should return distinct png data urls for VS Code and Zed',
    async () => {
      const vscode = await readAppBundleIcon('/Applications/Visual Studio Code.app');
      const zed = await readAppBundleIcon('/Applications/Zed.app');
      // 两枚图标都必须是真实 PNG data URL……
      expect(vscode?.startsWith('data:image/png;base64,')).toBe(true);
      expect(zed?.startsWith('data:image/png;base64,')).toBe(true);
      // ……且彼此不同（旧实现里两者是同一个 1598 字符的占位图）。
      expect(vscode).not.toBe(zed);
    },
  );
});