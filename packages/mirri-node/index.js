// @mirri-ai/mirri-node — Native Rust engine binding (NAPI cdylib).
//
// This file loads the compiled `.node` cdylib from the Rust build output.
// In development the file is copied next to this module by `scripts/build.mjs`;
// in a published package it ships as a platform-specific optional dependency.

'use strict';

const path = require('node:path');

// The .node file is placed alongside this index.js by the build script.
// We try the platform-specific name first, then fall back to a bare name.
const platformNames = [
  `mirri-node.${process.platform}-${process.arch}.node`,
  'mirri-node.node',
];

let binding = null;
let loadError = null;

for (const name of platformNames) {
  try {
    binding = require(path.join(__dirname, name));
    break;
  } catch (error) {
    // Keep the first error; a later name may succeed.
    if (loadError === null) loadError = error;
  }
}

if (binding === null) {
  // Allow a development fallback: load directly from the Rust target dir.
  const fs = require('node:fs');
  const rustTarget = path.resolve(__dirname, '../../rust/target');
  const candidates = [
    path.join(rustTarget, 'release', `libmirri_node${process.platform === 'win32' ? '.dll' : process.platform === 'darwin' ? '.dylib' : '.so'}`),
    path.join(rustTarget, 'debug', `libmirri_node${process.platform === 'win32' ? '.dll' : process.platform === 'darwin' ? '.dylib' : '.so'}`),
  ];
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) {
      try {
        binding = require(candidate);
        break;
      } catch (error) {
        if (loadError === null) loadError = error;
      }
    }
  }
}

if (binding === null) {
  throw new Error(
    `@mirri-ai/mirri-node: native binding not found. ` +
    `Run \`pnpm --filter @mirri-ai/mirri-node run build:cdylib\` to compile the Rust crate. ` +
    (loadError ? `Last error: ${loadError.message}` : ''),
  );
}

module.exports = binding;
