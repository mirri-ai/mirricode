#!/usr/bin/env node
// Packages a Rust release binary into the native distribution artifact.
//
// Output format matches the manifest/install-script contract: a flat zip
// archive containing the executable as `mirri` / `mirri.exe` at the archive
// root (mode 0o100755), plus a `.sha256` sidecar holding
// `<digest>  <basename>\n`. Artifacts land in `rust/dist-native/artifacts/`.
//
// Usage:
//   node rust/scripts/release-package.mjs --target aarch64-apple-darwin [--variant full|lite]
//                                          [--binary <repo-relative-path>]
//
// Variants:
//   full (default) — stage the web SPA into the rust-embed source directory,
//     build `mirri-cli` with its default features (SPA embedded) and package
//     it as `mirri-code-<nodeTarget>.zip` (the historical name, so
//     install.sh and the release manifest stay compatible).
//   lite — build `mirri-cli` with `--no-default-features` (no embedded SPA,
//     for remote servers reached through their own front end) and package
//     it as `mirri-code-<nodeTarget>-lite.zip`.
//
// Options:
//   --builder <build|zigbuild>  cargo subcommand used for the release build
//     (default `build`; `zigbuild` for the musl static targets — requires
//     cargo-zigbuild on PATH).
//   --binary <repo-relative-path>  overrides the default target-dir path and
//     skips both the staging and the build (useful for local verification
//     with a host build).
//
// The release build runs `--profile dist` (strict LTO settings) with
// --locked: a Cargo.lock out of sync with the manifests fails the
// packaging instead of silently producing an artifact from a
// resolved-but-uncommitted dependency set.
//
// macOS signing: when MIRRI_SIGN_IDENTITY is set and the target is darwin,
// the binary is codesigned (hardened runtime + entitlements, from
// rust/scripts/entitlements.plist) after the build and before the zip. An
// empty identity skips signing (local builds / repos without Apple secrets).

import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createReadStream, createWriteStream } from 'node:fs';
import { mkdir, stat, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import { pipeline } from 'node:stream/promises';

const rustDir = resolve(import.meta.dirname, '..');
const repoDir = resolve(rustDir, '..');
// The rust pipeline owns its own artifact directory. It used to write into
// `apps/mirri-code/dist-native/`, but that app is a frozen TypeScript package
// and nothing in this pipeline reads from it.
const artifactsDir = resolve(rustDir, 'dist-native/artifacts');
// Hardened-runtime entitlements for the darwin binaries. The rust binaries
// don't JIT-compile JS and don't dlopen third-party .node modules, but they DO
// spawn a packaged Node CLI and inherit the runtime, so the relaxed
// entitlements (JIT / unsigned executable memory / library validation / DYLD
// env vars) are kept: the artifact has to keep working when it execs Node.
const ENTITLEMENTS_PATH = resolve(rustDir, 'scripts/entitlements.plist');

// Rust target triple → the platform-arch name the release manifest and the
// install scripts use (`mirri-code-<target>.zip`).
const TARGET_MAP = new Map([
  ['x86_64-unknown-linux-musl', 'linux-x64'],
  ['aarch64-unknown-linux-musl', 'linux-arm64'],
  ['x86_64-apple-darwin', 'darwin-x64'],
  ['aarch64-apple-darwin', 'darwin-arm64'],
  ['x86_64-pc-windows-msvc', 'win32-x64'],
  ['aarch64-pc-windows-msvc', 'win32-arm64'],
]);

function fail(message) {
  console.error(message);
  process.exit(1);
}

function argValue(name) {
  return process.argv.find((arg, index) => index > 0 && process.argv[index - 1] === name);
}

function run(command, args, label) {
  const result = spawnSync(command, args, { stdio: 'inherit', cwd: repoDir });
  if (result.error !== undefined) {
    fail(`${label}: failed to run \`${command}\` (${result.error.message})`);
  }
  if (result.status !== 0) {
    fail(`${label}: \`${command} ${args.join(' ')}\` exited with ${result.status}`);
  }
}

const rustTarget = argValue('--target');
if (!rustTarget) {
  fail('missing --target <rust-target-triple> (e.g. aarch64-apple-darwin)');
}
const nodeTarget = TARGET_MAP.get(rustTarget);
if (!nodeTarget) {
  fail(
    `unknown rust target ${rustTarget}; supported: ${[...TARGET_MAP.keys()].join(', ')}`,
  );
}
const variant = argValue('--variant') ?? 'full';
if (variant !== 'full' && variant !== 'lite') {
  fail(`unknown --variant ${variant}; supported: full, lite`);
}
const builder = argValue('--builder') ?? 'build';
if (builder !== 'build' && builder !== 'zigbuild') {
  fail(`unknown --builder ${builder}; supported: build, zigbuild`);
}
const isWindows = rustTarget.endsWith('-windows-msvc');
const execName = isWindows ? 'mirri.exe' : 'mirri';
// --binary overrides the default target-dir path and skips staging and
// building (useful for local verification with a host build that omitted
// --target).
const binaryOverride = argValue('--binary');
const sourceBinary = binaryOverride
  ? resolve(repoDir, binaryOverride)
  : resolve(rustDir, 'target', rustTarget, 'dist', execName);

const artifactName =
  variant === 'lite'
    ? `mirri-code-${nodeTarget}-lite.zip`
    : `mirri-code-${nodeTarget}.zip`;
const artifactPath = resolve(artifactsDir, artifactName);
const checksumPath = `${artifactPath}.sha256`;

if (!binaryOverride) {
  // cargo must be pointed at the workspace manifest explicitly: this script
  // spawns it with cwd = repo root, where no Cargo.toml exists (the rust
  // workspace manifest lives at rust/Cargo.toml).
  const manifestArgs = ['--manifest-path', 'rust/Cargo.toml'];
  if (variant === 'full') {
    run('node', ['rust/scripts/stage-web.mjs'], 'staging the web SPA');
    run(
      'cargo',
      [builder, ...manifestArgs, '--profile', 'dist', '--locked', '--target', rustTarget, '-p', 'mirri-cli'],
      'building the full mirri binary',
    );
  } else {
    // lite variant, unchanged from before
    run(
      'cargo',
      [
        builder,
        ...manifestArgs,
        '--profile',
        'dist',
        '--locked',
        '--target',
        rustTarget,
        '-p',
        'mirri-cli',
        '--no-default-features',
      ],
      'building the lite mirri binary',
    );
  }
}

try {
  await stat(sourceBinary);
} catch {
  fail(
    `release binary not found at ${sourceBinary}. The ${variant} build should have produced it — check the cargo output above.`,
  );
}

// macOS signing hook: sign the binary with the Developer ID identity from
// MIRRI_SIGN_IDENTITY (hardened runtime + the entitlements the SEA pipeline
// used). Signing happens before the zip so the archive ships the signed
// binary. A signing failure fails the packaging — an unsigned darwin
// artifact must not silently reach the release.
const signIdentity = process.env.MIRRI_SIGN_IDENTITY;
if (rustTarget.endsWith('-apple-darwin')) {
  if (signIdentity) {
    run(
      'codesign',
      [
        '--force',
        '--options',
        'runtime',
        '--entitlements',
        ENTITLEMENTS_PATH,
        '--sign',
        signIdentity,
        sourceBinary,
      ],
      `signing ${execName} for ${nodeTarget}`,
    );
  } else {
    console.log(
      'MIRRI_SIGN_IDENTITY not set — skipping macOS codesign (unsigned darwin artifact)',
    );
  }
} else if (signIdentity) {
  console.log(
    `MIRRI_SIGN_IDENTITY is set but target ${rustTarget} is not darwin — skipping codesign`,
  );
}

await mkdir(artifactsDir, { recursive: true });

// yazl is declared by the root `package.json` (moved there when the TS CLI
// app was retired; it only ever existed for this packaging script).
const requireFromRoot = createRequire(resolve(repoDir, 'package.json'));
const { ZipFile } = requireFromRoot('yazl');

function sha256(path) {
  return new Promise((resolveHash, reject) => {
    const hash = createHash('sha256');
    const stream = createReadStream(path);
    stream.on('error', reject);
    stream.on('data', (chunk) => hash.update(chunk));
    stream.on('end', () => resolveHash(hash.digest('hex')));
  });
}

const zip = new ZipFile();
zip.addFile(sourceBinary, execName, { mode: 0o100755 });
zip.end();
await pipeline(zip.outputStream, createWriteStream(artifactPath));

const digest = await sha256(artifactPath);
await writeFile(checksumPath, `${digest}  ${basename(artifactPath)}\n`);

console.log(`Wrote native artifact: ${artifactPath}`);
console.log(`Wrote artifact checksum: ${checksumPath}`);
