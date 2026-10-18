import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { describe, expect, it } from 'vitest';

import {
  checkVersionSync,
  readAnchorVersion,
  readDesktopVersion,
  readRustWorkspaceVersion,
} from '../../../../rust/scripts/sync-version.mjs';

function makeTree(anchorVersion: string, rustVersion: string, desktopVersion: string) {
  const dir = mkdtempSync(join(tmpdir(), 'sync-version-'));
  mkdirSync(join(dir, 'packages/release'), { recursive: true });
  mkdirSync(join(dir, 'rust'), { recursive: true });
  mkdirSync(join(dir, 'apps/mirri-desktop'), { recursive: true });
  writeFileSync(
    join(dir, 'packages/release/package.json'),
    JSON.stringify({ name: '@mirri-ai/release', version: anchorVersion }),
  );
  writeFileSync(
    join(dir, 'rust/Cargo.toml'),
    '[workspace.package]\nversion = "' + rustVersion + '"\n',
  );
  writeFileSync(
    join(dir, 'apps/mirri-desktop/package.json'),
    JSON.stringify({ name: '@mirri-ai/mirri-desktop', version: desktopVersion }),
  );
  return dir;
}

describe('sync-version', () => {
  it('should read the anchor package version as the source', () => {
    const dir = makeTree('1.12.0', '1.12.0', '1.12.0');
    try {
      expect(readAnchorVersion(dir)).toBe('1.12.0');
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it('should report no problems when all versions match the anchor', () => {
    const dir = makeTree('1.12.0', '1.12.0', '1.12.0');
    try {
      expect(checkVersionSync({ repoDir: dir, rustDir: join(dir, 'rust') })).toEqual([]);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it('should report drift when rust workspace or desktop version diverges from the anchor', () => {
    const dir = makeTree('1.12.0', '1.11.0', '0.2.0');
    try {
      const problems = checkVersionSync({ repoDir: dir, rustDir: join(dir, 'rust') });
      expect(problems.length).toBeGreaterThanOrEqual(2);
      expect(problems.join('\n')).toContain('1.11.0');
      expect(problems.join('\n')).toContain('0.2.0');
      expect(readDesktopVersion(dir)).toBe('0.2.0');
      expect(readRustWorkspaceVersion(join(dir, 'rust'))).toBe('1.11.0');
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
