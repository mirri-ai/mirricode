import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    name: 'mirri-oauth',
    include: ['test/**/*.test.ts'],
  },
});
