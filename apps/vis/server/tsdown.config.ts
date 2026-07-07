import { defineConfig } from 'tsdown';

export default defineConfig({
  entry: { server: 'src/index.ts' },
  format: ['esm'],
  outDir: 'dist',
  clean: true,
  external: ['@mirri-ai/agent-core', '@mirri-ai/kosong', '@mirri-ai/kaos'],
});
