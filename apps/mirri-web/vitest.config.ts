import { defineConfig, mergeConfig } from 'vitest/config';
import viteConfig from './vite.config';

// The suite runs entirely on pure logic tests, but several cases re-import
// large modules through a fresh module registry. Under full-suite parallelism
// that takes longer than Vitest's 5 s default, so give every test headroom.
// The e2e suites still set their own, larger hook timeouts.
export default mergeConfig(
  viteConfig,
  defineConfig({
    test: {
      testTimeout: 15_000,
    },
  }),
);
