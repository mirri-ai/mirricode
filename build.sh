#!/bin/bash
# Mirri Code Quality Gate
# Runs: type check → tests → CLI build → desktop build
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

# Step 4: Run tests
echo "=== Step 3: Run tests ==="
pnpm run test
echo ""

# Step 5: Build CLI
echo "=== Step 4: Build CLI ==="
pnpm --filter @mirri-ai/mirri-code run build
echo ""

# Step 6: Build Desktop
echo "=== Step 5: Build Desktop ==="
pnpm --filter @mirri-ai/mirri-desktop run build
echo ""

echo "=========================================="
echo "  All steps passed! ✅"
echo "=========================================="
