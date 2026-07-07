#!/bin/bash
# Mirri Code Quality Gate
# Mirrors the CI pipeline: install → build packages → typecheck → lint → sherif → tests → pi-tui test → CLI build → smoke test → desktop build
set -e

cd "$(dirname "$0")"

echo "=========================================="
echo "  Mirri Code Quality Gate"
echo "=========================================="
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
echo "=== Step 5: Run tests ==="
pnpm run test
echo ""

# Step 7: Run pi-tui tests (separate because it uses node:test, not vitest)
echo "=== Step 6: Run pi-tui tests ==="
pnpm --filter @mirri-ai/pi-tui test
echo ""

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

echo "=========================================="
echo "  All steps passed! ✅"
echo "=========================================="
