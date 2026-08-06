import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    name: 'agent-profile',
    include: ['test/**/*.test.ts'],
  },
});
