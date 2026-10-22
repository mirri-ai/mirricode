#!/usr/bin/env bash
# Builds the rust `mirri` binary from this checkout and installs it over the
# one on PATH, so a local build can be tested without the release pipeline.
#
# This replaced `replace_cli.sh` + `apps/mirri-code/scripts/replace_on_mac.sh`,
# which built the retired TypeScript (Node SEA) CLI and installed *that* over
# the rust binary.
#
# Usage:
#   ./rust/scripts/replace-local-cli.sh              # build, then install
#   ./rust/scripts/replace-local-cli.sh --skip-build # install the last build
#   MIRRICODE_HOME=/tmp/mirri ./rust/scripts/replace-local-cli.sh
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

SKIP_BUILD=0
for arg in "$@"; do
  case "${arg}" in
    --skip-build) SKIP_BUILD=1 ;;
    *) echo "unknown argument: ${arg}" >&2; exit 2 ;;
  esac
done

binary="rust/target/dist/mirri"

if [[ "${SKIP_BUILD}" -eq 0 ]]; then
  echo "==> Building the web SPA (embedded into the binary)"
  pnpm --filter=@mirri-ai/mirri-web run build
  node rust/scripts/stage-web.mjs

  echo ""
  echo "==> Building mirri-cli (dist profile)"
  cargo build --manifest-path rust/Cargo.toml --profile dist --locked -p mirri-cli
fi

if [[ ! -f "${binary}" ]]; then
  echo "Build output not found: ${binary}" >&2
  echo "Run without --skip-build first." >&2
  exit 1
fi

if ! version_out="$("${binary}" --version 2>&1)"; then
  echo "Smoke test failed: \`mirri --version\` did not run." >&2
  printf '%s\n' "${version_out}" >&2
  exit 1
fi

home_dir="${MIRRICODE_HOME:-$HOME/.mirri-code}"
dst_dir="${home_dir}/bin"
dst="${dst_dir}/mirri"
mkdir -p "${dst_dir}"

if [[ -f "${dst}" ]]; then
  backup="${dst}.bak.$(date +%Y%m%d-%H%M%S)"
  cp "${dst}" "${backup}"
  echo "Backed up existing binary to ${backup}"
fi

cp "${binary}" "${dst}"
chmod 755 "${dst}"

if [[ "$(uname)" == "Darwin" ]]; then
  # Overwriting a binary that still carries the previous build's
  # `com.apple.provenance` xattr makes the kernel kill the new one at exec
  # (`zsh: killed`, exit 137) even when its signature is valid. Clear the
  # xattrs and re-sign ad-hoc so the signature matches the bytes on disk.
  xattr -cr "${dst}"
  codesign --sign - --force "${dst}" >/dev/null 2>&1

  # Capture the output first: `grep -q` under `pipefail` closes the pipe early
  # (SIGPIPE to codesign) and fails the pipeline on a good signature.
  verify_out="$(codesign --verify --verbose=2 "${dst}" 2>&1)"
  if ! grep -q "valid on disk" <<<"${verify_out}"; then
    echo "Signature verification failed for ${dst}" >&2
    printf '%s\n' "${verify_out}" >&2
    exit 1
  fi
fi

echo "Installed ${dst}"
echo "  source : ${binary}"
echo "  version: ${version_out}"
