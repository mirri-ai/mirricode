import { defineConfig } from 'tsdown';

import { rawTextPlugin } from '../../build/raw-text-plugin.mjs';

export default defineConfig({
  entry: ['./src/index.ts'],
  format: ['esm'],
  dts: true,
  outDir: 'dist',
  clean: true,
  plugins: [rawTextPlugin()],
  deps: {
    alwaysBundle: ['picomatch'],
    neverBundle: [
      '@mirri-ai/kosong',
      '@mirri-ai/kaos',
      '@mirri-ai/mirri-code-oauth',
    ],
  },
});
