import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    name: 'agent-core-v2',
    include: ['test/**/*.{test,e2e,integration}.ts'],
    exclude: ['test/_mirri-harness/**', 'node_modules/**'],
    setupFiles: ['test/setup.ts'],
    server: {
      deps: {
        // Inline workspace packages so their `exports` map subpaths
        // (e.g. `@mirri-ai/klient/memory`) resolve correctly under vitest.
        inline: [/@mirri-ai\/klient/],
      },
    },
  },
});
