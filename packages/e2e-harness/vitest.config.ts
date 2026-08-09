import { fileURLToPath } from 'node:url';

import { defineConfig } from 'vitest/config';

export default defineConfig({
  resolve: {
    alias: {
      '@mirri-ai/agent-core': fileURLToPath(
        new URL('../agent-core/src/index.ts', import.meta.url),
      ),
      '@mirri-ai/mirri-code-oauth': fileURLToPath(
        new URL('../oauth/src/index.ts', import.meta.url),
      ),
      '@mirri-ai/mirri-code-sdk': fileURLToPath(
        new URL('../node-sdk/src/index.ts', import.meta.url),
      ),
    },
  },
  test: {
    name: 'e2e-harness',
    env: {
      MIRRICODE_LOG_LEVEL: 'off',
    },
    include: ['test/**/*.test.ts'],
    testTimeout: 30_000,
  },
});
