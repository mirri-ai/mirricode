import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join } from 'pathe';

import { describe, expect, it } from 'vitest';

/** Collect .ts sources under a directory recursively. */
function collectTsFiles(dir: string): string[] {
  const files: string[] = [];
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) {
      files.push(...collectTsFiles(full));
    } else if (entry.endsWith('.ts')) {
      files.push(full);
    }
  }
  return files;
}

const SRC_ROOT = join(__dirname, '..', '..', 'src');

describe('TUI brand consistency', () => {
  it('should contain no old Kimi brand references when all TUI and constant sources are scanned', () => {
    const offenders: string[] = [];
    for (const file of [...collectTsFiles(join(SRC_ROOT, 'tui')), ...collectTsFiles(join(SRC_ROOT, 'constant'))]) {
      // Capital-K "Kimi" only: lowercase "kimi" is a model-family id prefix and is allowed.
      if (/Kimi/.test(readFileSync(file, 'utf8'))) offenders.push(file);
    }
    expect(offenders, `sources still reference the old brand:\n${offenders.join('\n')}`).toEqual([]);
  });
});