#!/bin/bash
# Replaces the installed mirri CLI with the latest native build output and
# re-signs it for macOS. Wraps apps/mirri-code/scripts/replace_on_mac.sh so
# you can run it from the repo root.
#
# Usage:
#   ./replace_cli.sh                         # skip build, use existing output (default)
#   ./replace_cli.sh --build                 # build native SEA, then replace
#   ./replace_cli.sh --skip-build            # skip build, use existing output
#   MIRRICODE_HOME=/tmp/mirri ./replace_cli.sh
set -e

cd "$(dirname "$0")"

SKIP_BUILD=1
for arg in "$@"; do
  case "$arg" in
    --build)      SKIP_BUILD=0 ;;
    --skip-build) SKIP_BUILD=1 ;;
  esac
done

if [[ "$(uname)" != "Darwin" ]]; then
  echo "replace_cli.sh only runs on macOS." >&2
  exit 1
fi

if [[ "${SKIP_BUILD}" -eq 0 ]]; then
  echo "==> Building native SEA binary"
  pnpm --filter @mirri-ai/mirri-code run build:native:sea
  echo ""
fi

exec apps/mirri-code/scripts/replace_on_mac.sh "$@"
