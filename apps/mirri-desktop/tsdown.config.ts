import { defineConfig } from 'tsdown';

// The Electron main process is loaded as CommonJS (`out/main.cjs`). All sources
// under src/main are bundled into a single file; `electron` stays external
// (provided by the Electron runtime) and Node built-ins are external by
// default.
//
// The preload bridge bundles to its own `out/preload.cjs` — it must be a
// separate file (referenced by webPreferences.preload) and stays fully
// self-contained so Electron's SANDBOXED preload can load it: the sandbox's
// require only resolves 'electron' and a few node built-ins, never local
// modules. Building main+preload as one multi-entry run makes rolldown
// code-split the shared src/main/ipc.ts into a chunk that preload.cjs then
// requires — which throws in the sandbox and silently drops
// window.mirriDesktop (the v1.11.0 regression). Two separate builds keep the
// preload single-file; src/main/preload-bundle.test.ts guards the invariant.
export default defineConfig([
  {
    entry: { main: 'src/main/index.ts' },
    format: ['cjs'],
    platform: 'node',
    target: 'node20',
    outDir: 'out',
    clean: true,
    dts: false,
    fixedExtension: true,
    deps: { neverBundle: ['electron'] },
  },
  {
    entry: { preload: 'src/main/preload.ts' },
    format: ['cjs'],
    platform: 'node',
    target: 'node20',
    outDir: 'out',
    clean: false, // do not wipe main.cjs built by the first entry
    dts: false,
    fixedExtension: true,
    deps: { neverBundle: ['electron'] },
  },
]);
