import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    name: 'agent-core',
    include: ['test/**/*.{test,e2e}.ts'],
  },
});
