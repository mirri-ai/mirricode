'use strict';

// electron-builder configuration.
//
// Signing / notarization are environment-driven so the same config produces
// either an unsigned local build or a fully signed + notarized distributable:
//
//   unsigned (default / local):
//     CSC_IDENTITY_AUTO_DISCOVERY=false  -> no signing, no notarization
//
//   signed + notarized (CI, with a Developer ID cert in the keychain):
//     MIRRI_DESKTOP_NOTARIZE=true
//     APPLE_API_KEY=<path to .p8>  APPLE_API_KEY_ID=<id>  APPLE_API_ISSUER=<id>
//
// The entitlements (hardened runtime) are applied to the app AND every nested
// Mach-O — including the bundled Mirri SEA backend — via entitlementsInherit, so
// the whole bundle passes notarization. Mirrors the TUI's native entitlements.

const notarize = process.env.MIRRI_DESKTOP_NOTARIZE === 'true';

const { version } = require('./package.json');

// Release artifact name: Mirri-Desktop-<version>-<target>.<ext>
// The version comes from package.json, which sync-version.mjs keeps aligned
// with the release anchor (@mirri-ai/release), so artifacts are traceable to
// the GitHub Release tag (vX.Y.Z).
const artifactName = `Mirri-Desktop-${version}-\${os}-\${arch}.\${ext}`;

module.exports = {
  appId: 'ai.mirri.desktop',
  productName: 'Mirri Code Desktop',
  copyright: 'Copyright © Mirri AI',

  directories: {
    output: 'dist-app',
  },

  // No native node modules in the Electron app itself; the backend is the
  // prebuilt SEA staged by before-pack.cjs. `build/icon.png` ships inside the
  // asar so the tray icon resolves from out/../build/icon.png in the packaged
  // app exactly like in dev.
  npmRebuild: false,
  asar: true,

  files: ['out/**', 'build/icon.png', 'package.json'],

  // Auto-update READINESS placeholder (P9 / D2): a generic-provider feed.
  // No real channel is configured — enabling an update check requires
  // MIRRICODE_DESKTOP_UPDATE_URL at runtime (see src/main/updater.ts), which
  // nothing sets today. Replace this URL when a distribution channel is
  // decided (user gate).
  publish: [{ provider: 'generic', url: 'https://updates.invalid/mirri-desktop' }],

  beforePack: './scripts/before-pack.cjs',
  extraResources: [{ from: 'resources-stage/bin', to: 'bin' }],

  mac: {
    category: 'public.app-category.developer-tools',
    hardenedRuntime: true,
    gatekeeperAssess: false,
    entitlements: 'build/entitlements.mac.plist',
    entitlementsInherit: 'build/entitlements.mac.plist',
    target: ['dmg', 'zip'],
    artifactName,
    notarize,
  },

  win: {
    target: ['nsis'],
    artifactName,
  },

  nsis: {
    oneClick: false,
    perMachine: false,
    allowToChangeInstallationDirectory: true,
  },

  linux: {
    category: 'Development',
    target: ['AppImage', 'deb'],
    artifactName,
    maintainer: 'Mirri AI',
  },
};
