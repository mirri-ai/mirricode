#!/bin/bash
# Mirri Code Quality Gate
# Mirrors the CI pipeline: install → build packages → typecheck → lint → sherif → tests → pi-tui test → CLI build → smoke test → desktop build → native binary build + smoke + package
# Pass --no-native to skip the native binary build.
set -e

cd "$(dirname "$0")"

# Parse flags
SKIP_NATIVE=0
SKIP_TESTS=0
for arg in "$@"; do
  case "$arg" in
    --no-native) SKIP_NATIVE=1 ;;
    --native) ;; # accepted for backwards compat, now the default
    -x|--skip-tests) SKIP_TESTS=1 ;;
  esac
done

echo "=========================================="
echo "  Mirri Code Quality Gate"
echo "=========================================="
echo ""

# Step 0: Clean build artifacts to prevent stale files from polluting the build
echo "=== Step -1: Clean build artifacts ==="
rm -rf apps/mirri-web/dist
rm -rf apps/mirri-code/dist-web
rm -rf apps/mirri-code/dist
rm -rf apps/mirri-code/dist-native
rm -rf apps/mirri-desktop/dist
rm -rf packages/*/dist
echo "✓ Cleaned dist directories"
echo ""

# Step 1: Install dependencies
echo "=== Step 0: Install dependencies ==="
pnpm install
echo ""

# Step 2: Build packages
echo "=== Step 1: Build packages ==="
pnpm run build:packages
echo ""

# Step 3: Type check
echo "=== Step 2: Type check ==="
pnpm run typecheck
echo ""

# Step 4: Lint
echo "=== Step 3: Lint ==="
pnpm run lint
echo ""

# Step 5: Sherif (monorepo consistency)
echo "=== Step 4: Sherif ==="
pnpm run sherif
echo ""

# Step 6: Run tests
if [ "$SKIP_TESTS" -ne 1 ]; then
  echo "=== Step 5: Run tests ==="
  pnpm run test
  echo ""

  # Step 7: Run pi-tui tests (separate because it uses node:test, not vitest)
  echo "=== Step 6: Run pi-tui tests ==="
  pnpm --filter @mirri-ai/pi-tui test
  echo ""
else
  echo "=== Step 5-6: Skipped (-x) ==="
  echo ""
fi

# Step 8: Build CLI
echo "=== Step 7: Build CLI ==="
pnpm --filter @mirri-ai/mirri-code run build
echo ""

# Step 9: Smoke test CLI bundle
echo "=== Step 8: Smoke test CLI ==="
pnpm -C apps/mirri-code run smoke
echo ""

# Step 10: Build Desktop
echo "=== Step 9: Build Desktop ==="
pnpm --filter @mirri-ai/mirri-desktop run build
echo ""

# Step 11: Nix build pre-flight checks
echo "=== Step 10: Nix build pre-flight checks ==="
NIX_CHECK_FAILED=0

# Check 1: Native build scripts exist
if [ ! -f "apps/mirri-code/scripts/native/build.mjs" ]; then
  echo "❌ FAIL: apps/mirri-code/scripts/native/build.mjs is missing"
  echo "   The nix build requires native build scripts. Run:"
  echo "   cp -r /path/to/mirri-code/apps/mirri-code/scripts/native apps/mirri-code/scripts/native"
  NIX_CHECK_FAILED=1
else
  echo "✓ Native build scripts exist"
fi

# Check 2: Package names in native-deps.mjs match @mirri-ai scope
if grep -q "@moonshot-ai/" apps/mirri-code/scripts/native/native-deps.mjs 2>/dev/null; then
  echo "❌ FAIL: native-deps.mjs contains @moonshot-ai/ references (should be @mirri-ai/)"
  grep -n "@moonshot-ai/" apps/mirri-code/scripts/native/native-deps.mjs
  NIX_CHECK_FAILED=1
else
  echo "✓ Package names in native-deps.mjs are correct"
fi

# Check 3: flake.nix fileset includes apps/mirri-code
if ! grep -q './apps/mirri-code' flake.nix 2>/dev/null; then
  echo "❌ FAIL: flake.nix does not include ./apps/mirri-code in fileset"
  NIX_CHECK_FAILED=1
else
  echo "✓ flake.nix includes apps/mirri-code"
fi

# Check 4: Try nix build if nix is available
if command -v nix &> /dev/null; then
  echo "Running nix build (this may take a while)..."
  if nix build .#mirri-code --dry-run 2>/dev/null; then
    echo "✓ nix build dry-run passed"
  else
    echo "⚠ nix build dry-run failed (run 'nix build .#mirri-code' for details)"
  fi
else
  echo "⚠ nix not installed, skipping nix build verification"
  echo "  Install nix: https://nixos.org/download.html"
fi

if [ $NIX_CHECK_FAILED -ne 0 ]; then
  echo ""
  echo "❌ Nix build pre-flight checks failed"
  exit 1
fi
echo ""

if [ "$SKIP_NATIVE" -ne 1 ]; then
  echo "=== Step 11: Build native binary ==="
  pnpm -C apps/mirri-code run build:native:sea
  TARGET="$(node -e "process.stdout.write(process.platform + '-' + process.arch)")"
  echo ""
  echo "Native binary: apps/mirri-code/dist-native/bin/${TARGET}/mirri"
  echo ""

  echo "=== Step 12: Native smoke test ==="
  pnpm -C apps/mirri-code run test:native:smoke
  echo ""

  echo "=== Step 13: Package native binary ==="
  pnpm -C apps/mirri-code run package:native
  echo ""
  echo "Packaged artifacts: apps/mirri-code/dist-native/artifacts/"
  echo ""
else
  echo "=== Step 11-13: Skipped (--no-native) ==="
  echo ""
fi

echo "=========================================="
echo "  All steps passed! ✅"
echo "=========================================="
