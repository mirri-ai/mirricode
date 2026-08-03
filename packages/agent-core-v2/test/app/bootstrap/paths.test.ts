import { existsSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, describe, expect, it } from 'vitest';

import { ensureMirriHome, resolveConfigPath, resolveMirriHome } from '#/app/bootstrap/bootstrap';

describe('bootstrap path helpers', () => {
  describe('resolveMirriHome', () => {
    it('uses explicit homeDir when provided', () => {
      expect(resolveMirriHome('/tmp/mirri')).toBe('/tmp/mirri');
    });

    it('falls back to MIRRICODE_HOME env', () => {
      const prev = process.env['MIRRICODE_HOME'];
      process.env['MIRRICODE_HOME'] = '/env/mirri';
      try {
        expect(resolveMirriHome()).toBe('/env/mirri');
      } finally {
        if (prev === undefined) delete process.env['MIRRICODE_HOME'];
        else process.env['MIRRICODE_HOME'] = prev;
      }
    });
  });

  describe('resolveConfigPath', () => {
    it('uses explicit configPath when provided', () => {
      expect(resolveConfigPath({ configPath: '/x/config.toml' })).toBe('/x/config.toml');
    });

    it('joins homeDir with config.toml', () => {
      expect(resolveConfigPath({ homeDir: '/tmp/mirri' })).toBe('/tmp/mirri/config.toml');
    });
  });

  describe('ensureMirriHome', () => {
    let dir: string | undefined;
    afterEach(() => {
      if (dir) rmSync(dir, { recursive: true, force: true });
    });

    it('creates the directory with 0700 permissions', () => {
      dir = join(mkdtempSync(join(tmpdir(), 'mirri-home-')), 'nested');
      ensureMirriHome(dir);
      expect(existsSync(dir)).toBe(true);
    });
  });
});
