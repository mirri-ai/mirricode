import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    name: 'v2-oauth',
    include: ['test/**/*.test.ts'],
  },
});
