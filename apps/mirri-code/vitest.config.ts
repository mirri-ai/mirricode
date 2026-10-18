import { resolve } from 'node:path';

import { defineConfig } from 'vitest/config';

const appRoot = import.meta.dirname;

export default defineConfig({
  resolve: {
    alias: {
      '@': resolve(appRoot, 'src'),
    },
  },
  test: {
    name: 'cli',
    env: {
      MIRRICODE_LOG_LEVEL: 'off',
      // Engine selection must be hermetic: an ambient MIRRICODE_RUST_ENGINE
      // or MIRRICODE_LEGACY_FLAG exported in the developer's shell would
      // reroute the mocked harness tests onto the real engines.
      MIRRICODE_RUST_ENGINE: '',
      MIRRICODE_LEGACY_FLAG: '',
    },
    include: ['test/**/*.test.ts', 'test/**/*.test.tsx'],
  },
});
