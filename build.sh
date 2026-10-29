#!/bin/bash
# Mirri Code v3 Quality Gate (Rust core + web + desktop)
#
# A scoped counterpart to build.sh that skips everything serving only the TS
# kernel: package builds (build:packages), repo-wide typecheck/lint/sherif, the
# TS package test suite, the CLI bundle + smoke test, and the CLI native SEA
# binary. What remains is exactly the v3 stack:
#
#   rust core (mirri-cli, web SPA embedded via rust-embed)
#     → web SPA build
#     → Electron desktop (backend staging + electron-builder DMG)
#
# The web/desktop typecheck and tests do NOT need `build:packages` first: the
# workspace packages they consume for tests (kap-server, e2e-harness,
# protocol) export their `src/*.ts` directly, and the production web bundle
# has zero workspace dependencies.
#
# Flags:
#   -x, --skip-tests   skip all test phases (rust workspace tests, clippy,
#                      zero-IO gate, web vitest, desktop vitest)
set -e

cd "$(dirname "$0")"

# Parse flags
SKIP_TESTS=0
for arg in "$@"; do
  case "$arg" in
    -x|--skip-tests) SKIP_TESTS=1 ;;
  esac
done

echo "=========================================="
echo "  Mirri Code v3 Quality Gate"
echo "  (rust core + web + desktop)"
echo "=========================================="
echo ""

# Step 0: Clean build artifacts to prevent stale files from polluting the build
echo "=== Step 0: Clean build artifacts ==="
rm -rf apps/mirri-web/dist
rm -rf apps/mirri-desktop/dist
rm -rf apps/mirri-desktop/dist-app
rm -rf apps/mirri-desktop/out
echo "✓ Cleaned dist directories (rust/target kept for incremental builds)"
echo ""

# Step 1: Install dependencies
echo "=== Step 1: Install dependencies ==="
pnpm install
echo ""

# Step 2: Type check (web + desktop only — no package builds needed)
echo "=== Step 2: Type check (web + desktop) ==="
pnpm --filter @mirri-ai/mirri-web run typecheck
pnpm --filter @mirri-ai/mirri-desktop run typecheck
echo ""

# Step 3: Tests
if [ "$SKIP_TESTS" -ne 1 ]; then
  echo "=== Step 3: Rust workspace tests ==="
  cargo test --manifest-path rust/Cargo.toml --workspace
  echo ""

  echo "=== Step 4: Rust clippy (deny warnings) ==="
  cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings
  echo ""

  echo "=== Step 5: Rust zero-IO gate ==="
  pnpm run gate:rust-zero-io
  echo ""

  # The web e2e suites drive the real rust server via rust/target/debug/mirri
  # (they self-skip with MIRRICODE_E2E_SKIP_RUST=1 when it is missing), so
  # build the debug binary first to make them actually run.
  echo "=== Step 6: Build rust debug binary (for web e2e) ==="
  cargo build --manifest-path rust/Cargo.toml -p mirri-cli
  echo ""

  echo "=== Step 7: Web tests (vitest, incl. rust-backend e2e) ==="
  pnpm -C apps/mirri-web run test
  echo ""

  echo "=== Step 8: Desktop tests (vitest) ==="
  pnpm --filter @mirri-ai/mirri-desktop exec vitest run
  echo ""
else
  echo "=== Step 3-8: Skipped (-x) ==="
  echo ""
fi

# Step 9: Build the rust backend with the SPA embedded
# (builds the web SPA → stages it into the rust-embed source dir →
#  cargo build --profile dist --locked -p mirri-cli → stages the binary into
#  apps/mirri-desktop/build/backend/<platform>-<arch>/)
echo "=== Step 9: Build rust backend (SPA embedded) ==="
pnpm -C apps/mirri-desktop run build:backend
echo ""
echo "Backend binary: apps/mirri-desktop/build/backend/$(node -e "process.stdout.write(process.platform + '-' + process.arch)")/mirri"
echo ""

# Step 10: Build the desktop DMG (electron-builder)
# Produces an unsigned .dmg/.zip (and installers for the current platform) in
# apps/mirri-desktop/dist-app/. before-pack.cjs stages the backend binary from
# Step 9 into the app's resources.
echo "=== Step 10: Build desktop installer ==="
pnpm --filter @mirri-ai/mirri-desktop run dist
echo ""
echo "Desktop artifacts: apps/mirri-desktop/dist-app/"
echo ""

# Step 11: Install the all-in-one CLI binary (serve / web / print / acp)
# into the user's Mirri bin directory, mirroring the packaged CLI layout.
echo "=== Step 11: Install CLI binary to ~/.mirri-code/bin ==="
MIRRI_BACKEND_DIR="apps/mirri-desktop/build/backend/$(node -e "process.stdout.write(process.platform + '-' + process.arch)")"
mkdir -p ~/.mirri-code/bin
cp "$MIRRI_BACKEND_DIR/mirri" ~/.mirri-code/bin/mirri
chmod +x ~/.mirri-code/bin/mirri
echo "Installed: ~/.mirri-code/bin/mirri"
echo ""

echo "=========================================="
echo "  All steps passed! ✅"
echo "  Finished at: $(date '+%Y-%m-%d %H:%M:%S')"
echo "=========================================="
