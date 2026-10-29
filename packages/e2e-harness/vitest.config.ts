import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    name: 'e2e-harness',
    env: {
      MIRRICODE_LOG_LEVEL: 'off',
    },
    include: ['test/**/*.test.ts'],
    testTimeout: 30_000,
  },
});
