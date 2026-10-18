import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { describe, expect, it } from 'vitest';

// Resolve from this file (apps/mirri-code/test/native/) so the test passes
// regardless of the cwd pnpm --filter exec picks for vitest.
const scriptPath = join(import.meta.dirname, '../../scripts/native/produce-manifest.mjs');

function runManifest(dir: string, tag: string) {
  execFileSync('node', [scriptPath, dir, tag]);
  return JSON.parse(readFileSync(join(dir, 'manifest.json'), 'utf-8'));
}

function fakeSums(dir: string, filename: string) {
  writeFileSync(join(dir, `${filename}.sha256`), `${'a'.repeat(64)}  ${filename}\n`);
}

describe('produce-manifest', () => {
  it('should keep the legacy platforms section unchanged when only mirri-code zips are present', () => {
    const dir = mkdtempSync(join(tmpdir(), 'produce-manifest-'));
    try {
      fakeSums(dir, 'mirri-code-darwin-arm64.zip');
      fakeSums(dir, 'mirri-code-linux-x64.zip');
      const manifest = runManifest(dir, 'v1.12.0');
      expect(manifest.version).toBe('1.12.0');
      expect(Object.keys(manifest.platforms).toSorted()).toEqual(['darwin-arm64', 'linux-x64']);
      expect(manifest.platforms['darwin-arm64']).toEqual({
        filename: 'mirri-code-darwin-arm64.zip',
        checksum: 'a'.repeat(64),
      });
      expect(manifest.acp).toBeUndefined();
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it('should emit the acp section when mirri-acp zips are present', () => {
    const dir = mkdtempSync(join(tmpdir(), 'produce-manifest-'));
    try {
      fakeSums(dir, 'mirri-code-darwin-arm64.zip');
      fakeSums(dir, 'mirri-acp-darwin-arm64.zip');
      fakeSums(dir, 'mirri-acp-win32-x64.zip');
      const manifest = runManifest(dir, 'v1.12.0');
      expect(Object.keys(manifest.acp ?? {}).toSorted()).toEqual(['darwin-arm64', 'win32-x64']);
      expect(manifest.acp?.['win32-x64']).toEqual({
        filename: 'mirri-acp-win32-x64.zip',
        checksum: 'a'.repeat(64),
      });
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it('should normalize legacy scoped tags to a bare version', () => {
    const dir = mkdtempSync(join(tmpdir(), 'produce-manifest-'));
    try {
      fakeSums(dir, 'mirri-code-darwin-arm64.zip');
      const manifest = runManifest(dir, '@mirri-ai/mirri-code@1.11.0');
      expect(manifest.version).toBe('1.11.0');
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
