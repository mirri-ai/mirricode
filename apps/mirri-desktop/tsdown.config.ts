import { defineConfig } from 'tsdown';

// The Electron main process is loaded as CommonJS (`out/main.cjs`). All sources
// under src/main are bundled into a single file; `electron` stays external
// (provided by the Electron runtime) and Node built-ins are external by
// default. The preload bridge bundles to its own `out/preload.cjs` — it must
// be a separate file (referenced by webPreferences.preload) and stays fully
// self-contained so Electron's sandboxed preload can load it without a
// runtime `require` of local modules.
export default defineConfig({
  entry: { main: 'src/main/index.ts', preload: 'src/main/preload.ts' },
  format: ['cjs'],
  platform: 'node',
  target: 'node20',
  outDir: 'out',
  clean: true,
  dts: false,
  fixedExtension: true,
  deps: { neverBundle: ['electron'] },
});
