#!/bin/bash
# Performs a full native build (SEA + smoke + package) and replaces the
# installed mirri CLI with the output, re-signing it for macOS.
#
# Usage:
#   ./replace_cli.sh                         # full native build, then replace (default)
#   ./replace_cli.sh --skip-build            # skip build, use existing output
#   MIRRICODE_HOME=/tmp/mirri ./replace_cli.sh
set -e

cd "$(dirname "$0")"

SKIP_BUILD=0
for arg in "$@"; do
  case "$arg" in
    --skip-build) SKIP_BUILD=1 ;;
  esac
done

if [[ "$(uname)" != "Darwin" ]]; then
  echo "replace_cli.sh only runs on macOS." >&2
  exit 1
fi

if [[ "${SKIP_BUILD}" -eq 0 ]]; then
  echo "==> Building native SEA binary"
  pnpm -C apps/mirri-code run build:native:sea
  echo ""

  echo "==> Native smoke test"
  pnpm -C apps/mirri-code run test:native:smoke
  echo ""

  echo "==> Packaging native binary"
  pnpm -C apps/mirri-code run package:native
  echo ""
fi

exec apps/mirri-code/scripts/replace_on_mac.sh
