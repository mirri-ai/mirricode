import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { describe, expect, it } from 'vitest';

import { createMirriHarnessRust } from '#/index';

describe('TUI init smoke (Rust engine)', () => {
  it('should pass through TUI initialization path when engine is available', async () => {
    const homeDir = mkdtempSync(join(tmpdir(), 'mirri-tui-smoke-'));
    const harness = createMirriHarnessRust({ homeDir, uiMode: 'cli' });

    try {
      await harness.ensureConfigFile();
      const config = await harness.getConfig();
      expect(config).toBeDefined();

      const diag = await harness.getConfigDiagnostics();
      expect(diag).toBeDefined();

      const sessions = await harness.listSessions();
      expect(sessions).toHaveLength(0);
    } finally {
      await harness.close();
    }
  });
});
