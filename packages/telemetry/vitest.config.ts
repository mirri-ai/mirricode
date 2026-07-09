import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    name: 'mirri-telemetry',
    include: ['test/**/*.test.ts'],
  },
});
