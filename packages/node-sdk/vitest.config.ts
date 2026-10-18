import { fileURLToPath } from 'node:url';

import { defineConfig } from 'vitest/config';

export default defineConfig({
  resolve: {
    alias: {
      '@mirri-ai/agent-core': fileURLToPath(new URL('../agent-core/src/index.ts', import.meta.url)),
      '@mirri-ai/e2e-harness': fileURLToPath(
        new URL('../e2e-harness/src/index.ts', import.meta.url),
      ),
    },
  },
  test: {
    name: 'mirri-sdk',
    env: {
      MIRRICODE_LOG_LEVEL: 'off',
    },
    include: ['test/**/*.test.ts'],
    testTimeout: 30_000,
  },
});
