#!/bin/bash
# Mirri Code v3 Quality Gate (Rust core + web + desktop)
#
# Local counterpart to the CI workflow: it runs the same check surface as
# `.github/workflows/ci.yml` where that surface applies locally, then builds
# and packages the v3 stack:
#
#   static gates (pnpm gate, oxlint, root script tests, version check,
#                 cargo-deny, typecheck) → rust core tests + clippy → web
#   vitest incl. real-engine e2e → desktop vitest → rust core (mirri-cli, web
#   SPA embedded via rust-embed) → Electron desktop (backend staging +
#   electron-builder DMG)
#
# Three intentional differences from CI:
#   - `pnpm install` runs without `--frozen-lockfile` so a local dependency
#     edit does not break the build; CI keeps the lockfile frozen.
#   - `cargo deny` is skipped when the binary is not installed (CI installs it
#     in its own job); the skip is reported in the final summary.
# The web e2e suites run here but are skipped in CI's `pnpm test` — see the
# Step 6/7 comment.
#
# The web/desktop typecheck and tests do NOT need `build:packages` first: the
# workspace packages they consume for tests (e2e-harness, protocol) export
# their `src/*.ts` directly, and the production web bundle has zero workspace
# dependencies.
#
# Flags:
#   -x, --skip-tests   skip all test phases (unified gate, oxlint, root script
#                      tests, version check, cargo-deny, rust workspace tests,
#                      clippy, web vitest, desktop vitest)
set -e

cd "$(dirname "$0")"

# Parse flags
SKIP_TESTS=0
# Set to 1 when cargo-deny is not installed; reported in the final summary so a
# skipped RustSec/license gate is never silently green.
DENY_SKIPPED=0
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

# Step 2: Type check (packages + web + desktop)
# The packages sweep is `tsc -p --noEmit` only, no `build:packages` — this is
# the local equivalent of the CI `typecheck` job's tsgo pass over
# packages/*/tsconfig.json. desktop has no CI equivalent and stays stricter.
echo "=== Step 2: Type check (packages + web + desktop) ==="
pnpm -r --filter './packages/*' run typecheck
pnpm --filter @mirri-ai/mirri-web run typecheck
pnpm --filter @mirri-ai/mirri-desktop run typecheck
echo ""

# Step 3: Static gates
if [ "$SKIP_TESTS" -ne 1 ]; then
  echo "=== Step 3: Static gates (pnpm gate + oxlint + repo checks) ==="
  # `pnpm gate` is the single source of truth for the fast source-scan gates:
  # service-naming, sherif, rust zero-IO, spawn hygiene, test hygiene. Do not
  # enumerate them here — scripts/gate.mjs owns the stage set and ordering.
  #
  # Exit codes: 0 = green, 1 = new breakage (fatal), 2 = only entries from the
  # KNOWN_FAILURES ledger failed. Known debt is tracked, so it must not block a
  # build — but it is echoed so the run is never quietly green.
  gate_status=0
  pnpm gate || gate_status=$?
  if [ "$gate_status" -eq 2 ]; then
    echo "⚠ pnpm gate reported known-failure(s) only (see KNOWN_FAILURES in scripts/gate.mjs)"
  elif [ "$gate_status" -ne 0 ]; then
    echo "✗ pnpm gate failed (exit $gate_status)"
    exit "$gate_status"
  fi
  # oxlint --type-aware is 0-errors-only, same as the CI `lint` job; the repo
  # currently carries warnings and they are reported, not fatal.
  pnpm run lint
  # scripts/*.test.mjs run on node:test, so `pnpm test` never collects them.
  node --test scripts/*.test.mjs rust/scripts/*.test.mjs
  node rust/scripts/sync-version.mjs --check

  # cargo-deny is a separately installed binary (`cargo install cargo-deny
  # --locked`); CI enforces it in the `rust-deny` job. A missing local tool
  # must not block a build, but the skip is surfaced in the final summary.
  if cargo deny --version >/dev/null 2>&1; then
    (cd rust && cargo deny check --all-features)
  else
    echo "⚠ cargo-deny not installed — skipping RustSec/license/source gate"
    echo "  install with: cargo install cargo-deny --locked"
    DENY_SKIPPED=1
  fi
  echo ""

  echo "=== Step 4: Rust workspace tests ==="
  cargo test --manifest-path rust/Cargo.toml --workspace --locked
  echo ""

  echo "=== Step 5: Rust clippy (deny warnings) ==="
  cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets --locked -- -D warnings
  echo ""

  # The web e2e suites drive the real rust server via rust/target/debug/mirri
  # (they self-skip with MIRRICODE_E2E_SKIP_RUST=1 when it is missing), so
  # build the debug binary first to make them actually run.
  echo "=== Step 6: Build rust debug binary (for web e2e) ==="
  cargo build --manifest-path rust/Cargo.toml -p mirri-cli --locked
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
if [ "$DENY_SKIPPED" -eq 1 ]; then
  echo "  NOTE: cargo-deny gate was SKIPPED (binary not installed)"
fi
echo "  Finished at: $(date '+%Y-%m-%d %H:%M:%S')"
echo "=========================================="
