import { fileURLToPath } from 'node:url';

import { defineConfig } from 'vitest/config';

export default defineConfig({
  resolve: {
    alias: {
      '@mirri-ai/e2e-harness': fileURLToPath(
        new URL('../e2e-harness/src/index.ts', import.meta.url),
      ),
    },
  },
  test: {
    name: 'acp-adapter',
    include: ['test/**/*.test.ts'],
    testTimeout: 30_000,
  },
});
