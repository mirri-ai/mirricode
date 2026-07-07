import { defineConfig } from 'tsdown';

export default defineConfig({
  entry: ['./src/index.ts'],
  format: ['esm'],
  dts: true,
  outDir: 'dist',
  clean: true,
  deps: {
    neverBundle: [
      '@agentclientprotocol/sdk',
      '@mirri-ai/agent-core',
      '@mirri-ai/mirri-code-sdk',
      '@mirri-ai/kosong',
      '@mirri-ai/kaos',
    ],
  },
});
